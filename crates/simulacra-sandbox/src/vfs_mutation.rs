//! Batched VFS mutations with capability checks, byte-budget reservation,
//! journalling, and rollback on failure.
//!
//! [`AgentCell::apply_vfs_mutations`] is the entry point. The `remove_path` and
//! `move_path` convenience methods are thin wrappers over it.

use std::sync::Arc;

use simulacra_types::{FsMutation, VfsError, VirtualFs};

use crate::SandboxError;
use crate::guards::{check_and_journal_capability, release_vfs_bytes, reserve_vfs_bytes};
use crate::vfs_batch::push_batch_items;
use crate::vfs_mutation_journal::{
    journal_deletes, journal_execution_failure, journal_moves, journal_plan,
    journal_unknown_outcome, journal_writes,
};
use crate::vfs_state::{
    VfsPathState, VfsRollbackExpectation, capture_vfs_rollback_entries,
    ensure_parent_components_are_directories, ensure_vfs_path_missing, record_missing_parent_dirs,
    restore_vfs_path_states, upsert_rollback_expectation, validate_file_matches,
    validate_write_precondition,
};
use crate::{AgentCell, cap_name_for_read, cap_name_for_write};

/// A VFS mutation batch item executed through [`AgentCell`].
#[derive(Debug, Clone)]
pub enum VfsMutation {
    Write {
        path: String,
        data: Vec<u8>,
        precondition: VfsWritePrecondition,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    MoveAndWrite {
        from: String,
        to: String,
        data: Vec<u8>,
        from_precondition: Option<Vec<u8>>,
    },
}

/// Expected state for a [`VfsMutation::Write`] before it may execute.
#[derive(Debug, Clone)]
pub enum VfsWritePrecondition {
    Any,
    Missing,
    Matches(Vec<u8>),
}

#[derive(Debug, Clone)]
enum PreparedVfsMutation {
    Write {
        path: String,
        data: Vec<u8>,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
        data: Vec<u8>,
    },
    MoveAndWrite {
        from: String,
        to: String,
        data: Vec<u8>,
    },
}

impl AgentCell {
    /// Apply a set of VFS mutations as a single capability-checked, journaled batch.
    ///
    /// The batch commits atomically where the filesystem supports it; it runs
    /// item by item with rollback only as the `Unsupported` fallback.
    pub fn apply_vfs_mutations(
        &self,
        tool_name: &str,
        mutations: &[VfsMutation],
    ) -> Result<(), SandboxError> {
        let _span = tracing::info_span!(
            "sandbox_apply_vfs_mutations",
            simulacra.operation.name = "sandbox_apply_vfs_mutations",
            simulacra.tool.name = tool_name,
            simulacra.vfs.mutation_count = mutations.len() as u64,
        )
        .entered();

        let _mutation_guard = self
            .vfs_mutation_lock
            .lock()
            .map_err(|e| SandboxError::Internal(format!("vfs mutation mutex poisoned: {e}")))?;

        check_batch_capabilities(self, tool_name, mutations)?;
        self.preflight_vfs_write_bytes(0)?;

        let mut write_bytes = 0_u64;
        let mut file_write_entries = Vec::new();
        let mut file_delete_entries = Vec::new();
        let mut file_move_entries = Vec::new();
        for mutation in mutations {
            match mutation {
                VfsMutation::Write { path, data, .. } => {
                    write_bytes = write_bytes.saturating_add(data.len() as u64);
                    file_write_entries.push((path.as_str(), data.len() as u64));
                }
                VfsMutation::Delete { path } => {
                    file_delete_entries.push(path.as_str());
                }
                VfsMutation::Move { from, to } => {
                    let metadata = self.vfs.metadata(from).map_err(SandboxError::Vfs)?;
                    if !metadata.is_file {
                        return Err(SandboxError::Vfs(VfsError::NotAFile(from.clone())));
                    }
                    write_bytes = write_bytes.saturating_add(metadata.size);
                    file_move_entries.push((from.as_str(), to.as_str()));
                    file_write_entries.push((to.as_str(), metadata.size));
                }
                VfsMutation::MoveAndWrite { from, to, data, .. } => {
                    write_bytes = write_bytes.saturating_add(data.len() as u64);
                    file_move_entries.push((from.as_str(), to.as_str()));
                    file_write_entries.push((to.as_str(), data.len() as u64));
                }
            }
        }

        if write_bytes > 0 {
            reserve_vfs_bytes(&self.budget, write_bytes, &self.journal, &self.agent_id)?;
        }

        let prepared = match prepare_all_mutations(self, mutations) {
            Ok(prepared) => prepared,
            Err(err) => {
                release_reserved_bytes(self, write_bytes)?;
                return Err(err);
            }
        };

        let rollback_states = match capture_vfs_rollback_entries(&self.vfs, &prepared.rollback) {
            Ok(states) => states,
            Err(err) => {
                release_reserved_bytes(self, write_bytes)?;
                return Err(SandboxError::Vfs(err));
            }
        };

        if let Err(err) = journal_plan(self, tool_name, mutations.len()) {
            release_reserved_bytes(self, write_bytes)?;
            return Err(err);
        }
        if let Err(err) = journal_deletes(self, &file_delete_entries) {
            release_reserved_bytes(self, write_bytes)?;
            return Err(err);
        }
        if let Err(err) = journal_moves(self, &file_move_entries) {
            release_reserved_bytes(self, write_bytes)?;
            return Err(err);
        }
        if let Err(err) = journal_writes(self, &file_write_entries) {
            release_reserved_bytes(self, write_bytes)?;
            return Err(err);
        }

        match self.vfs.apply_batch(&prepared.batch) {
            Ok(()) => {}
            Err(VfsError::Unsupported(_)) => {
                // No atomic batches on this filesystem: execute item by item
                // and compensate on failure.
                if let Err(err) = execute_prepared(&self.vfs, &prepared.mutations) {
                    if matches!(err, VfsError::OutcomeUnknown(_)) {
                        // The write may still land: keep the reservation and
                        // do not compensate, a rollback would race it.
                        journal_unknown_outcome(self, tool_name, &err);
                        return Err(SandboxError::Vfs(err));
                    }
                    release_reserved_bytes(self, write_bytes)?;
                    if let Err(restore_err) =
                        restore_vfs_path_states(&self.vfs, &rollback_states, &prepared.parent_dirs)
                    {
                        return Err(SandboxError::Internal(format!(
                            "failed to roll back VFS mutations after {err}: {restore_err}"
                        )));
                    }
                    journal_execution_failure(self, tool_name, &err);
                    return Err(SandboxError::Vfs(err));
                }
            }
            Err(err @ VfsError::OutcomeUnknown(_)) => {
                // The batch may still land: keep the reservation (over-counting
                // is the safe direction) and do not roll back.
                journal_unknown_outcome(self, tool_name, &err);
                return Err(SandboxError::Vfs(err));
            }
            Err(err) => {
                // An atomic batch that failed changed nothing: no rollback.
                release_reserved_bytes(self, write_bytes)?;
                journal_execution_failure(self, tool_name, &err);
                return Err(SandboxError::Vfs(err));
            }
        }

        Ok(())
    }

    /// Remove a VFS path, checking path write capability first.
    pub fn remove_path(&self, path: &str) -> Result<(), SandboxError> {
        self.apply_vfs_mutations(
            "remove_path",
            &[VfsMutation::Delete {
                path: path.to_string(),
            }],
        )
    }

    /// Move a VFS file, checking read capability for the source and write
    /// capability for both paths.
    pub fn move_path(&self, from: &str, to: &str) -> Result<(), SandboxError> {
        self.apply_vfs_mutations(
            "move_path",
            &[VfsMutation::Move {
                from: from.to_string(),
                to: to.to_string(),
            }],
        )
    }
}

/// Roll back the byte reservation if any were reserved; no-op otherwise.
fn release_reserved_bytes(cell: &AgentCell, write_bytes: u64) -> Result<(), SandboxError> {
    if write_bytes > 0 {
        release_vfs_bytes(&cell.budget, write_bytes)?;
    }
    Ok(())
}

/// Capability-check every mutation in the batch up front.
fn check_batch_capabilities(
    cell: &AgentCell,
    tool_name: &str,
    mutations: &[VfsMutation],
) -> Result<(), SandboxError> {
    for mutation in mutations {
        match mutation {
            VfsMutation::Write { path, .. } | VfsMutation::Delete { path } => {
                check_and_journal_capability(
                    || cell.capability.check_path_write(path),
                    tool_name,
                    cap_name_for_write(path),
                    &cell.journal,
                    &cell.agent_id,
                )?;
            }
            VfsMutation::Move { from, to } | VfsMutation::MoveAndWrite { from, to, .. } => {
                check_and_journal_capability(
                    || cell.capability.check_path_read(from),
                    tool_name,
                    cap_name_for_read(from),
                    &cell.journal,
                    &cell.agent_id,
                )?;
                check_and_journal_capability(
                    || cell.capability.check_path_write(from),
                    tool_name,
                    cap_name_for_write(from),
                    &cell.journal,
                    &cell.agent_id,
                )?;
                check_and_journal_capability(
                    || cell.capability.check_path_write(to),
                    tool_name,
                    cap_name_for_write(to),
                    &cell.journal,
                    &cell.agent_id,
                )?;
            }
        }
    }
    Ok(())
}

/// Everything prepared for a batch: rollback bookkeeping for the item-by-item
/// fallback, the items for that fallback, and the atomic batch.
struct Prepared {
    rollback: Vec<VfsRollbackExpectation>,
    parent_dirs: Vec<String>,
    mutations: Vec<PreparedVfsMutation>,
    batch: Vec<FsMutation>,
}

/// Validate preconditions and capture rollback metadata for every mutation.
fn prepare_all_mutations(
    cell: &AgentCell,
    mutations: &[VfsMutation],
) -> Result<Prepared, SandboxError> {
    let mut prepared = Prepared {
        rollback: Vec::new(),
        parent_dirs: Vec::new(),
        mutations: Vec::with_capacity(mutations.len()),
        batch: Vec::with_capacity(mutations.len()),
    };
    for mutation in mutations {
        // Pin the revision before prepare_mutation reads the bytes it validates.
        push_batch_items(&cell.vfs, mutation, &mut prepared.batch)?;
        prepare_mutation(
            cell,
            mutation,
            &mut prepared.rollback,
            &mut prepared.parent_dirs,
            &mut prepared.mutations,
        )?;
    }
    Ok(prepared)
}

#[allow(clippy::too_many_arguments)]
fn prepare_mutation(
    cell: &AgentCell,
    mutation: &VfsMutation,
    rollback_expectations: &mut Vec<VfsRollbackExpectation>,
    created_parent_dirs: &mut Vec<String>,
    prepared_mutations: &mut Vec<PreparedVfsMutation>,
) -> Result<(), SandboxError> {
    match mutation {
        VfsMutation::Write {
            path,
            data,
            precondition,
        } => {
            ensure_parent_components_are_directories(&cell.vfs, path)?;
            validate_write_precondition(&cell.vfs, path, precondition)?;
            upsert_rollback_expectation(
                rollback_expectations,
                path,
                VfsPathState::File(data.clone()),
            );
            record_missing_parent_dirs(&cell.vfs, path, created_parent_dirs);
            prepared_mutations.push(PreparedVfsMutation::Write {
                path: path.clone(),
                data: data.clone(),
            });
        }
        VfsMutation::Delete { path } => {
            require_file(&cell.vfs, path)?;
            upsert_rollback_expectation(rollback_expectations, path, VfsPathState::Missing);
            prepared_mutations.push(PreparedVfsMutation::Delete { path: path.clone() });
        }
        VfsMutation::Move { from, to } => {
            require_file(&cell.vfs, from)?;
            ensure_parent_components_are_directories(&cell.vfs, to)?;
            ensure_vfs_path_missing(&cell.vfs, to)?;
            let data = cell.vfs.read(from).map_err(SandboxError::Vfs)?;
            upsert_rollback_expectation(rollback_expectations, from, VfsPathState::Missing);
            upsert_rollback_expectation(
                rollback_expectations,
                to,
                VfsPathState::File(data.clone()),
            );
            record_missing_parent_dirs(&cell.vfs, to, created_parent_dirs);
            prepared_mutations.push(PreparedVfsMutation::Move {
                from: from.clone(),
                to: to.clone(),
                data,
            });
        }
        VfsMutation::MoveAndWrite {
            from,
            to,
            data,
            from_precondition,
        } => {
            require_file(&cell.vfs, from)?;
            if let Some(expected) = from_precondition {
                validate_file_matches(&cell.vfs, from, expected)?;
            }
            ensure_parent_components_are_directories(&cell.vfs, to)?;
            ensure_vfs_path_missing(&cell.vfs, to)?;
            upsert_rollback_expectation(rollback_expectations, from, VfsPathState::Missing);
            upsert_rollback_expectation(
                rollback_expectations,
                to,
                VfsPathState::File(data.clone()),
            );
            record_missing_parent_dirs(&cell.vfs, to, created_parent_dirs);
            prepared_mutations.push(PreparedVfsMutation::MoveAndWrite {
                from: from.clone(),
                to: to.clone(),
                data: data.clone(),
            });
        }
    }
    Ok(())
}

/// Run the prepared mutations against the VFS.
fn execute_prepared(
    vfs: &Arc<dyn VirtualFs>,
    prepared: &[PreparedVfsMutation],
) -> Result<(), VfsError> {
    for mutation in prepared {
        match mutation {
            PreparedVfsMutation::Write { path, data } => vfs.write(path, data)?,
            PreparedVfsMutation::Delete { path } => vfs.remove(path)?,
            PreparedVfsMutation::Move { from, to, data }
            | PreparedVfsMutation::MoveAndWrite { from, to, data, .. } => {
                vfs.write(to, data)?;
                vfs.remove(from)?;
            }
        }
    }
    Ok(())
}

/// Require that `path` exists and is a file.
fn require_file(vfs: &Arc<dyn VirtualFs>, path: &str) -> Result<(), SandboxError> {
    let metadata = vfs.metadata(path).map_err(SandboxError::Vfs)?;
    if !metadata.is_file {
        return Err(SandboxError::Vfs(VfsError::NotAFile(path.to_string())));
    }
    Ok(())
}
