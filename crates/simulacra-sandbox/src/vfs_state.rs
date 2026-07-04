//! Low-level VFS state queries, precondition validation, and rollback helpers
//! used by the batched mutation pipeline in [`crate::vfs_mutation`].

use std::sync::Arc;

use simulacra_types::{FsMetadata, VfsError, VirtualFs};

use crate::SandboxError;

pub(super) fn ensure_vfs_path_missing(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
) -> Result<(), SandboxError> {
    if vfs.exists(path) {
        return Err(SandboxError::Vfs(VfsError::AlreadyExists(path.to_string())));
    }
    Ok(())
}

pub(super) fn ensure_parent_components_are_directories(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
) -> Result<(), SandboxError> {
    for parent in parent_paths(path) {
        match vfs.metadata(&parent) {
            Ok(metadata) if metadata.is_file => {
                return Err(SandboxError::Vfs(VfsError::NotADirectory(parent)));
            }
            Ok(_) | Err(VfsError::NotFound(_)) => {}
            Err(err) => return Err(SandboxError::Vfs(err)),
        }
    }
    Ok(())
}

pub(super) fn validate_file_matches(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
    expected: &[u8],
) -> Result<(), SandboxError> {
    let metadata = vfs.metadata(path).map_err(SandboxError::Vfs)?;
    if !metadata.is_file {
        return Err(SandboxError::Vfs(VfsError::NotAFile(path.to_string())));
    }
    let current = vfs.read(path).map_err(SandboxError::Vfs)?;
    if current != expected {
        return Err(SandboxError::Vfs(VfsError::Io(format!(
            "stale write precondition failed for {path}"
        ))));
    }
    Ok(())
}

pub(super) fn parent_paths(path: &str) -> Vec<String> {
    let Some((parent, _name)) = path.rsplit_once('/') else {
        return Vec::new();
    };
    let parent = if parent.is_empty() { "/" } else { parent };
    let mut paths = Vec::new();
    let mut current = String::new();
    for segment in parent.split('/').filter(|segment| !segment.is_empty()) {
        current.push('/');
        current.push_str(segment);
        paths.push(current.clone());
    }
    paths
}

pub(super) fn record_missing_parent_dirs(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
    created_parent_dirs: &mut Vec<String>,
) {
    for parent in parent_paths(path) {
        if !vfs.exists(&parent)
            && !created_parent_dirs
                .iter()
                .any(|existing| existing == &parent)
        {
            created_parent_dirs.push(parent);
        }
    }
}

/// Validate that a write's precondition holds against the current VFS state.
pub(super) fn validate_write_precondition(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
    precondition: &crate::VfsWritePrecondition,
) -> Result<(), SandboxError> {
    match precondition {
        crate::VfsWritePrecondition::Any => {
            if let Ok(FsMetadata { is_dir: true, .. }) = vfs.metadata(path) {
                return Err(SandboxError::Vfs(VfsError::NotAFile(path.to_string())));
            }
            Ok(())
        }
        crate::VfsWritePrecondition::Missing => ensure_vfs_path_missing(vfs, path),
        crate::VfsWritePrecondition::Matches(expected) => {
            validate_file_matches(vfs, path, expected)
        }
    }
}

/// A snapshot of a VFS path's state, used for rollback bookkeeping.
#[derive(Debug, Clone)]
pub(super) enum VfsPathState {
    Missing,
    File(Vec<u8>),
    Dir,
}

/// The state a path was in before a batch ran, plus the state it should land
/// in after the batch. If the actual post-batch state already matches the
/// expected end state, the rollback is a no-op for that path.
#[derive(Debug, Clone)]
pub(super) struct VfsRollbackEntry {
    pub(super) path: String,
    pub(super) before: VfsPathState,
    pub(super) expected_after: VfsPathState,
}

#[derive(Debug, Clone)]
pub(super) struct VfsRollbackExpectation {
    pub(super) path: String,
    pub(super) expected_after: VfsPathState,
}

pub(super) fn upsert_rollback_expectation(
    expectations: &mut Vec<VfsRollbackExpectation>,
    path: &str,
    expected_after: VfsPathState,
) {
    if let Some(existing) = expectations.iter_mut().find(|e| e.path == path) {
        existing.expected_after = expected_after;
    } else {
        expectations.push(VfsRollbackExpectation {
            path: path.to_string(),
            expected_after,
        });
    }
}

pub(super) fn capture_vfs_rollback_entries(
    vfs: &Arc<dyn VirtualFs>,
    expectations: &[VfsRollbackExpectation],
) -> Result<Vec<VfsRollbackEntry>, VfsError> {
    let mut entries = Vec::with_capacity(expectations.len());
    for expectation in expectations {
        let path = &expectation.path;
        let state = match vfs.metadata(path) {
            Ok(metadata) if metadata.is_file => VfsPathState::File(vfs.read(path)?),
            Ok(metadata) if metadata.is_dir => VfsPathState::Dir,
            Ok(_) => VfsPathState::Missing,
            Err(VfsError::NotFound(_)) => VfsPathState::Missing,
            Err(err) => return Err(err),
        };
        entries.push(VfsRollbackEntry {
            path: path.clone(),
            before: state,
            expected_after: expectation.expected_after.clone(),
        });
    }
    Ok(entries)
}

fn vfs_path_matches_state(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
    expected: &VfsPathState,
) -> Result<bool, VfsError> {
    match expected {
        VfsPathState::Missing => Ok(!vfs.exists(path)),
        VfsPathState::File(expected_data) => match vfs.metadata(path) {
            Ok(metadata) if metadata.is_file => Ok(vfs.read(path)? == *expected_data),
            Ok(_) | Err(VfsError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        },
        VfsPathState::Dir => match vfs.metadata(path) {
            Ok(metadata) => Ok(metadata.is_dir),
            Err(VfsError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        },
    }
}

pub(super) fn restore_vfs_path_states(
    vfs: &Arc<dyn VirtualFs>,
    states: &[VfsRollbackEntry],
    created_parent_dirs: &[String],
) -> Result<(), VfsError> {
    for entry in states.iter().rev() {
        if !vfs_path_matches_state(vfs, &entry.path, &entry.expected_after)? {
            continue;
        }
        match &entry.before {
            VfsPathState::Missing => {
                if vfs.exists(&entry.path) {
                    vfs.remove(&entry.path)?;
                }
            }
            VfsPathState::File(data) => {
                vfs.write(&entry.path, data)?;
            }
            VfsPathState::Dir => {
                vfs.mkdir(&entry.path)?;
            }
        }
    }

    for path in created_parent_dirs.iter().rev() {
        if matches!(vfs.list_dir(path), Ok(entries) if entries.is_empty()) {
            vfs.remove(path)?;
        }
    }

    Ok(())
}
