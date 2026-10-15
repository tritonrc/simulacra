//! Conditional append for the mediated JS `fs`: read, then write only if
//! nothing changed in between, retrying once so a concurrent writer's bytes
//! are not overwritten, where the filesystem has revisions.

use simulacra_types::{Precondition, VfsError, VirtualFs};

/// Revision of `path` if it has one. `Err` when the metadata call failed.
fn revision(vfs: &dyn VirtualFs, path: &str) -> Result<Option<u64>, VfsError> {
    vfs.metadata(path).map(|meta| meta.revision)
}

/// What the append may assume about the bytes it read.
enum Observed {
    /// The bytes and the precondition that pins them to the commit.
    Stable(Vec<u8>, Precondition),
    /// A writer landed while we were reading; read again.
    Changed,
}

fn observe(vfs: &dyn VirtualFs, path: &str) -> Result<Observed, VfsError> {
    // Revision before the bytes, and again after: equal revisions mean the
    // bytes belong to that revision.
    let before = revision(vfs, path);
    let existing = match vfs.read(path) {
        Ok(existing) => existing,
        Err(VfsError::NotFound(_)) => {
            return Ok(Observed::Stable(Vec::new(), Precondition::MustBeAbsent));
        }
        Err(err) => return Err(err),
    };
    let after = revision(vfs, path);
    let precondition = match (before, after) {
        (Ok(Some(b)), Ok(Some(a))) if a == b => Precondition::MustMatch(a),
        (Ok(None), Ok(None)) => Precondition::Unconditional,
        (Err(VfsError::NotFound(_)), _) | (_, Err(VfsError::NotFound(_))) => {
            return Ok(Observed::Changed);
        }
        (Err(err), _) | (_, Err(err)) => return Err(err),
        _ => return Ok(Observed::Changed),
    };
    Ok(Observed::Stable(existing, precondition))
}

const RACED: &str = "append raced with another writer twice";

/// How one attempt ended.
enum Attempt {
    Done,
    /// A concurrent writer got in between the read and the write.
    Raced,
}

fn attempt(vfs: &dyn VirtualFs, path: &str, data: &[u8]) -> Result<Attempt, VfsError> {
    let (mut combined, precondition) = match observe(vfs, path) {
        Ok(Observed::Stable(existing, precondition)) => (existing, precondition),
        Ok(Observed::Changed) => return Ok(Attempt::Raced),
        Err(err) => return Err(err),
    };
    combined.extend_from_slice(data);
    match vfs.write_if(path, &combined, precondition) {
        Ok(()) => Ok(Attempt::Done),
        // No conditional writes here: the old, racy behaviour.
        Err(VfsError::Unsupported(_)) => vfs.write(path, &combined).map(|()| Attempt::Done),
        Err(VfsError::Conflict(_)) => Ok(Attempt::Raced),
        Err(err) => Err(err),
    }
}

/// Append `data` to `path`, creating it if missing.
///
/// Where the filesystem has revisions, the write is conditional on the
/// revision the bytes were read at, so a concurrent writer's bytes are not
/// overwritten: the append is retried once, then fails. Without revisions the
/// write is unconditional and can lose a concurrent writer's bytes. A failure
/// that is `OutcomeUnknown` may still have landed, and stays typed so the
/// caller keeps its budget reservation.
pub(crate) fn append_conditionally(
    vfs: &dyn VirtualFs,
    path: &str,
    data: &[u8],
) -> Result<(), VfsError> {
    match attempt(vfs, path, data)? {
        Attempt::Done => return Ok(()),
        Attempt::Raced => {}
    }
    match attempt(vfs, path, data)? {
        Attempt::Done => Ok(()),
        Attempt::Raced => Err(VfsError::Conflict(RACED.to_string())),
    }
}
