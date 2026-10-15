//! Translation of prepared [`VfsMutation`]s into one atomic [`FsMutation`]
//! batch, with each precondition pinned to the revision seen at prepare time.

use std::sync::Arc;

use simulacra_types::{FsMutation, Precondition, VirtualFs};

use crate::SandboxError;
use crate::vfs_mutation::{VfsMutation, VfsWritePrecondition};

/// Pin `path` to the revision it has now. A filesystem that reports no
/// revision yields `Unconditional`; a metadata failure is an error, never a
/// silently unconditional commit.
///
/// Ordering matters: call this BEFORE reading the bytes the caller validates.
/// The revision then names a state no newer than the validated bytes, so a
/// write landing in between makes the commit conflict. Reading it after the
/// bytes would pin whatever landed in the gap and overwrite it.
fn revision_or_unconditional(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
) -> Result<Precondition, SandboxError> {
    let metadata = vfs.metadata(path).map_err(SandboxError::Vfs)?;
    Ok(metadata
        .revision
        .map_or(Precondition::Unconditional, Precondition::MustMatch))
}

/// Append the atomic-batch items for one mutation. Call before validating the
/// mutation's bytes (see [`revision_or_unconditional`]).
pub(crate) fn push_batch_items(
    vfs: &Arc<dyn VirtualFs>,
    mutation: &VfsMutation,
    batch: &mut Vec<FsMutation>,
) -> Result<(), SandboxError> {
    match mutation {
        VfsMutation::Write {
            path,
            data,
            precondition,
        } => {
            let precondition = match precondition {
                VfsWritePrecondition::Any => Precondition::Unconditional,
                VfsWritePrecondition::Missing => Precondition::MustBeAbsent,
                VfsWritePrecondition::Matches(_) => revision_or_unconditional(vfs, path)?,
            };
            batch.push(FsMutation::Write {
                path: path.clone(),
                data: data.clone(),
                precondition,
            });
        }
        VfsMutation::Delete { path } => batch.push(FsMutation::Remove {
            path: path.clone(),
            precondition: revision_or_unconditional(vfs, path)?,
        }),
        VfsMutation::Move { from, to } => batch.push(FsMutation::Rename {
            from: from.clone(),
            to: to.clone(),
            precondition: revision_or_unconditional(vfs, from)?,
        }),
        VfsMutation::MoveAndWrite { from, to, data, .. } => {
            batch.push(FsMutation::Write {
                path: to.clone(),
                data: data.clone(),
                precondition: Precondition::MustBeAbsent,
            });
            batch.push(FsMutation::Remove {
                path: from.clone(),
                precondition: revision_or_unconditional(vfs, from)?,
            });
        }
    }
    Ok(())
}
