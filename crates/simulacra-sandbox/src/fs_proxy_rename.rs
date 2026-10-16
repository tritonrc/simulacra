//! Node-style `rename` for the mediated JS `fs`: an existing file at the
//! destination is replaced, in one atomic batch where the filesystem has
//! them, and a concurrent writer of the destination is not silently lost,
//! where the filesystem has revisions.

use simulacra_types::{FsMutation, Precondition, VfsError, VirtualFs};
use simulacra_vfs::normalize_path;

use crate::fs_proxy::AgentCellFsProxy;

/// Writes one copied file: path and bytes.
pub(crate) type CopyWrite<'a> = dyn Fn(&str, &[u8]) -> Result<(), VfsError> + 'a;

impl AgentCellFsProxy {
    /// One file of the rename's copy fallback: its bytes are reserved against
    /// the write budget before anything is copied, journaled like a write,
    /// and released on a definite failure; an `OutcomeUnknown` keeps them
    /// charged. The write capability was judged on the rename's endpoints,
    /// and the copied descendants are covered by that same decision.
    pub(crate) fn charged_copy_write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.reserve_write_budget(data.len())
            .map_err(VfsError::PermissionDenied)?;
        self.journal_file_write(path, data.len());
        if let Err(err) = self.vfs.write(path, data) {
            self.release_write_budget(&err, data.len())
                .map_err(VfsError::PermissionDenied)?;
            return Err(err);
        }
        Ok(())
    }
}

/// Rename `from` to `to`, replacing an existing file at `to`.
///
/// Order of judgement: `from` must exist (so a missing source is reported as
/// such, whatever `to` is); renaming a path onto itself is then a no-op; a
/// directory at `to`, or a directory replacing a file, is a `Conflict`.
///
/// On a filesystem with batches and revisions, the replacement removes `to`
/// only if it still has the revision that was read, so a writer landing in
/// between makes the batch fail with nothing changed. A failure that is
/// `OutcomeUnknown` may still have landed. Without revisions the removal is
/// unconditional, and a residual window remains: `to` may be replaced, even
/// by a directory, between the read and the removal.
///
/// The source is pinned to the revision read with `from`'s metadata, so a
/// directory swapped in for the file that was inspected does not match and the
/// batch fails.
///
/// A filesystem that cannot rename or batch atomically gets the old
/// copy-then-remove emulation, which overwrites and is not atomic. Every file
/// it copies goes through `write`, so the caller can charge the bytes against
/// the write budget and journal them like any other write; a refusal there
/// happens before that file is copied.
pub(crate) fn rename_replacing(
    vfs: &dyn VirtualFs,
    from: &str,
    to: &str,
    write: &CopyWrite<'_>,
) -> Result<(), VfsError> {
    let source = vfs.metadata(from)?;
    if normalize_path(from) == normalize_path(to) {
        return Ok(());
    }
    let atomic = match vfs.metadata(to) {
        Err(VfsError::NotFound(_)) => vfs.rename(from, to),
        Err(err) => return Err(err),
        Ok(target) if target.is_dir => {
            return Err(VfsError::Conflict(format!(
                "destination is a directory: {to}"
            )));
        }
        Ok(_) if source.is_dir => {
            return Err(VfsError::Conflict(format!(
                "cannot replace a file with a directory: {to}"
            )));
        }
        Ok(target) => {
            // The revision pins the replacement to the file we looked at.
            let remove = target
                .revision
                .map_or(Precondition::Unconditional, Precondition::MustMatch);
            // The source is pinned too: a directory swapped in for the file
            // that was inspected does not match, so it cannot replace `to`.
            let pin = source
                .revision
                .map_or(Precondition::Unconditional, Precondition::MustMatch);
            vfs.apply_batch(&[
                FsMutation::Remove {
                    path: to.to_string(),
                    precondition: remove,
                },
                FsMutation::Rename {
                    from: from.to_string(),
                    to: to.to_string(),
                    precondition: pin,
                },
            ])
        }
    };
    match atomic {
        Err(VfsError::Unsupported(_)) => {
            copy_vfs_entry(vfs, from, to, write)?;
            vfs.remove(from)
        }
        result => result,
    }
}

fn join_vfs_path(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), child)
    }
}

fn copy_vfs_entry(
    vfs: &dyn VirtualFs,
    from: &str,
    to: &str,
    write: &CopyWrite<'_>,
) -> Result<(), VfsError> {
    let meta = vfs.metadata(from)?;
    if meta.is_file {
        let data = vfs.read(from)?;
        return write(to, &data);
    }

    vfs.mkdir(to)?;
    for child in vfs.list_dir(from)? {
        let child_from = join_vfs_path(from, &child);
        let child_to = join_vfs_path(to, &child);
        copy_vfs_entry(vfs, &child_from, &child_to, write)?;
    }
    Ok(())
}
