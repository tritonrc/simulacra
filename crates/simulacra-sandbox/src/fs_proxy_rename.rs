//! Node-style `rename` for the mediated JS `fs`: an existing file at the
//! destination is replaced, in one atomic batch where the filesystem has
//! them, and a concurrent writer of the destination is not silently lost,
//! where the filesystem has revisions.

use simulacra_types::{FsMutation, Precondition, VfsError, VirtualFs};
use simulacra_vfs::normalize_path;

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
/// A filesystem that cannot rename or batch atomically gets the old
/// copy-then-remove emulation, which overwrites and is not atomic.
pub(crate) fn rename_replacing(vfs: &dyn VirtualFs, from: &str, to: &str) -> Result<(), VfsError> {
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
            vfs.apply_batch(&[
                FsMutation::Remove {
                    path: to.to_string(),
                    precondition: remove,
                },
                FsMutation::Rename {
                    from: from.to_string(),
                    to: to.to_string(),
                    precondition: Precondition::Unconditional,
                },
            ])
        }
    };
    match atomic {
        Err(VfsError::Unsupported(_)) => {
            copy_vfs_entry(vfs, from, to)?;
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

fn copy_vfs_entry(vfs: &dyn VirtualFs, from: &str, to: &str) -> Result<(), VfsError> {
    let meta = vfs.metadata(from)?;
    if meta.is_file {
        let data = vfs.read(from)?;
        return vfs.write(to, &data);
    }

    vfs.mkdir(to)?;
    for child in vfs.list_dir(from)? {
        let child_from = join_vfs_path(from, &child);
        let child_to = join_vfs_path(to, &child);
        copy_vfs_entry(vfs, &child_from, &child_to)?;
    }
    Ok(())
}
