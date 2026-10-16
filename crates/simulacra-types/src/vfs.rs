use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::TenantId;
use crate::vfs_ops::{DirListing, FsMutation, Precondition};

/// Metadata about a file or directory in the virtual filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsMetadata {
    pub is_file: bool,
    pub is_dir: bool,
    pub size: u64,
    /// What `Precondition::MustMatch` compares against. Only backends that
    /// version entries set it; a revision never names different contents twice.
    #[serde(default)]
    pub revision: Option<u64>,
    /// The entry's media type, where the backend records one.
    #[serde(default)]
    pub media_type: Option<String>,
}

impl FsMetadata {
    pub fn file(size: u64) -> Self {
        Self {
            is_file: true,
            is_dir: false,
            size,
            revision: None,
            media_type: None,
        }
    }

    pub fn dir() -> Self {
        Self {
            is_file: false,
            is_dir: true,
            size: 0,
            revision: None,
            media_type: None,
        }
    }

    pub fn with_revision(mut self, revision: u64) -> Self {
        self.revision = Some(revision);
        self
    }
}

/// A serializable snapshot of all VFS state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfsSnapshot {
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VfsEvent {
    Written {
        tenant: TenantId,
        path: PathBuf,
        len: u64,
    },
    Removed {
        tenant: TenantId,
        path: PathBuf,
    },
    Skipped {
        count: u64,
    },
}

#[derive(Debug)]
pub struct VfsWatcher {
    receiver: broadcast::Receiver<VfsEvent>,
    prefix: String,
}

impl VfsWatcher {
    pub fn new(receiver: broadcast::Receiver<VfsEvent>, prefix: impl Into<String>) -> Self {
        Self {
            receiver,
            prefix: prefix.into(),
        }
    }

    pub fn dead(prefix: impl Into<String>) -> Self {
        let (sender, receiver) = broadcast::channel(1);
        drop(sender);
        Self::new(receiver, prefix)
    }

    pub async fn recv(&mut self) -> Option<VfsEvent> {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match self.receiver.recv().await {
                Ok(event) => {
                    // `Skipped` events surface unconditionally — the prefix
                    // filter is a convenience for path-bearing events only.
                    match &event {
                        VfsEvent::Skipped { .. } => return Some(event),
                        VfsEvent::Written { path, .. } | VfsEvent::Removed { path, .. } => {
                            if path_matches_prefix(path, &self.prefix) {
                                return Some(event);
                            }
                            // Non-matching event — silently consume and loop.
                        }
                    }
                }
                Err(RecvError::Closed) => return None,
                Err(RecvError::Lagged(n)) => {
                    return Some(VfsEvent::Skipped { count: n });
                }
            }
        }
    }
}

/// Returns true if `path` is "under" `prefix` for VfsWatcher prefix filtering.
///
/// Matching is **segment-aware** (per S039 §Behavior):
///   - `""` and `"/"` match all paths (consumer wants every event).
///   - Otherwise `path` matches `prefix` iff `path == prefix` OR `path` begins
///     with `prefix` followed by `/`. This means `subscribe("/foo")` accepts
///     `/foo` and `/foo/bar` but rejects `/foobar` (which is a different path,
///     not a child of `/foo`).
///   - A trailing slash on `prefix` is stripped before matching, so
///     `subscribe("/foo/")` is equivalent to `subscribe("/foo")`.
fn path_matches_prefix(path: &std::path::Path, prefix: &str) -> bool {
    if prefix.is_empty() || prefix == "/" {
        return true;
    }
    let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
    if prefix.is_empty() {
        // Pathological "//"-only input collapses to root.
        return true;
    }
    let s = path.to_string_lossy();
    if s.as_ref() == prefix {
        return true;
    }
    if let Some(rest) = s.strip_prefix(prefix) {
        return rest.starts_with('/');
    }
    false
}

#[cfg(test)]
mod path_matches_prefix_tests {
    use super::path_matches_prefix;
    use std::path::Path;

    #[test]
    fn empty_prefix_matches_anything() {
        assert!(path_matches_prefix(Path::new("/foo/bar"), ""));
        assert!(path_matches_prefix(Path::new("/"), ""));
    }

    #[test]
    fn root_prefix_matches_anything() {
        assert!(path_matches_prefix(Path::new("/foo/bar"), "/"));
        assert!(path_matches_prefix(Path::new("/anything"), "/"));
    }

    #[test]
    fn segment_aware_prefix_accepts_exact_match() {
        assert!(path_matches_prefix(Path::new("/foo"), "/foo"));
    }

    #[test]
    fn segment_aware_prefix_accepts_child_paths() {
        assert!(path_matches_prefix(Path::new("/foo/bar"), "/foo"));
        assert!(path_matches_prefix(Path::new("/foo/bar/baz"), "/foo"));
    }

    #[test]
    fn segment_aware_prefix_rejects_sibling_with_shared_byte_prefix() {
        // The whole point of the fix: `/foobar` is NOT under `/foo`.
        assert!(!path_matches_prefix(Path::new("/foobar"), "/foo"));
        assert!(!path_matches_prefix(Path::new("/foobar/baz"), "/foo"));
    }

    #[test]
    fn trailing_slash_on_prefix_is_normalized() {
        assert!(path_matches_prefix(Path::new("/foo"), "/foo/"));
        assert!(path_matches_prefix(Path::new("/foo/bar"), "/foo/"));
        assert!(!path_matches_prefix(Path::new("/foobar"), "/foo/"));
    }

    #[test]
    fn unicode_paths_match_segment_aware() {
        // Non-ASCII bytes inside a path segment must still match correctly:
        // `/résumé` should match prefix `/résumé`, and `/résumés` (different
        // segment) should not match `/résumé`.
        assert!(path_matches_prefix(
            Path::new("/résumé/draft.md"),
            "/résumé"
        ));
        assert!(path_matches_prefix(Path::new("/résumé"), "/résumé"));
        assert!(!path_matches_prefix(Path::new("/résumés"), "/résumé"));
    }
}

/// Errors from VFS operations.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("not a file: {0}")]
    NotAFile(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// A `VfsWrite` hook returned `Verdict::Deny` and the write was blocked.
    /// `reason` is the verbatim string the hook supplied.
    #[error("hook denied: {reason}")]
    HookDenied { reason: String },
    /// A `VfsWrite` hook returned `Verdict::Kill`. The agent run is being
    /// terminated; `reason` is the verbatim string the hook supplied.
    #[error("hook killed: {reason}")]
    HookKilled { reason: String },
    /// A `VfsWrite` hook returned a `Verdict::Continue(Some(ctx))` whose
    /// modified context violates the v1 mutation contract. The only field
    /// honored by `Verdict::Continue` is `path`; mutating `tenant` is a
    /// security pitfall and produces this error.
    #[error("hook contract violation")]
    HookContractViolation,
    /// This filesystem cannot do the operation atomically. A caller may fall
    /// back to a non-atomic emulation only where that is acceptable and it
    /// knows it is giving up atomicity (as `AgentCellFsProxy::rename` and
    /// `AgentCell::apply_vfs_mutations` do today).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A precondition did not hold, or a target already exists.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The write would exceed a storage limit.
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    /// Nothing changed: the deadline passed before the commit was sent.
    #[error("deadline exceeded: {0}")]
    DeadlineExceeded(String),
    /// The commit was sent and no answer came. It may still land.
    #[error("outcome unknown: {0}")]
    OutcomeUnknown(String),
    /// The filesystem was restored to an earlier snapshot after this handle
    /// opened; nothing changed. Reopen to write.
    #[error("filesystem restored: {0}")]
    FilesystemRestored(String),
}

/// Virtual filesystem trait. Object-safe.
/// All paths are rooted at `/`. No escape from the virtual root.
pub trait VirtualFs: Send + Sync + 'static {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError>;
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError>;
    fn exists(&self, path: &str) -> bool;
    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError>;
    fn mkdir(&self, path: &str) -> Result<(), VfsError>;
    fn remove(&self, path: &str) -> Result<(), VfsError>;
    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError>;
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError>;
    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError>;
    fn subscribe(&self, prefix: &str) -> VfsWatcher {
        VfsWatcher::dead(prefix)
    }

    /// Write only if `precondition` holds, checked and applied as one step.
    /// The default refuses: a check-then-write built from `metadata` and
    /// `write` would race, and callers must know when that is all they get.
    ///
    /// Errors: a failed precondition is `Conflict`; a directory at `path` is
    /// `NotAFile`; a file where a parent directory is needed is
    /// `NotADirectory`. Missing parents are created, as `write` does.
    fn write_if(
        &self,
        path: &str,
        _data: &[u8],
        _precondition: Precondition,
    ) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(format!("write_if {path}")))
    }

    /// Move a file or directory in one step.
    ///
    /// Errors: `from` missing is `NotFound`; `to` existing is `Conflict`;
    /// moving a directory into itself is `Io`; a file where a parent directory
    /// of `to` is needed is `NotADirectory`. As an `apply_batch` item, its
    /// precondition applies to `from`.
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        Err(VfsError::Unsupported(format!("rename {from} -> {to}")))
    }

    /// Apply every mutation or none. Items apply in order and each sees the
    /// effects of the earlier ones.
    ///
    /// Any error means nothing in the batch changed, except `OutcomeUnknown`,
    /// where the batch may still land. `FilesystemRestored` means the tree was
    /// restored since this handle opened, and nothing changed.
    fn apply_batch(&self, _mutations: &[FsMutation]) -> Result<(), VfsError> {
        Err(VfsError::Unsupported("apply_batch".to_owned()))
    }

    /// List a directory, possibly cut short by the backend's listing bound.
    /// The default never truncates.
    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        Ok(DirListing::complete(self.list_dir(path)?))
    }
}

/// Blanket impl so `Arc<dyn VirtualFs>` can be used wherever `V: VirtualFs` is expected.
impl VirtualFs for std::sync::Arc<dyn VirtualFs> {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        (**self).read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        (**self).write(path, data)
    }
    fn exists(&self, path: &str) -> bool {
        (**self).exists(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        (**self).list_dir(path)
    }
    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        (**self).mkdir(path)
    }
    fn remove(&self, path: &str) -> Result<(), VfsError> {
        (**self).remove(path)
    }
    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        (**self).metadata(path)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        (**self).snapshot()
    }
    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        (**self).restore(snapshot)
    }
    fn subscribe(&self, prefix: &str) -> VfsWatcher {
        (**self).subscribe(prefix)
    }
    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: Precondition,
    ) -> Result<(), VfsError> {
        (**self).write_if(path, data, precondition)
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        (**self).rename(from, to)
    }
    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        (**self).apply_batch(mutations)
    }
    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        (**self).list_dir_bounded(path)
    }
}
