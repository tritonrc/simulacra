//! Core `read_file`/`write_file` logic following the Golden Rule:
//! span → capability → budget → journal → execute → return.
//!
//! Shared by the `AgentCell` methods and the `AgentCellFsProxy` so that JS host
//! functions (`fs.readFileSync`, `fs.writeFileSync`, `simulacra:fs`) go through
//! the same enforcement chain as direct tool calls.

use std::sync::{Arc, Mutex};

use simulacra_types::{
    AgentId, CapabilityToken, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind,
    JournalStorage, ResourceBudget, VirtualFs,
};

use crate::SandboxError;
use crate::guards::{journal_budget_exhaustion, release_vfs_bytes, reserve_vfs_bytes};
use crate::{cap_name_for_read, cap_name_for_write, check_and_journal_capability};

/// Core read_file logic: span → capability → budget → execute → journal → return.
pub(crate) fn read_file_inner(
    path: &str,
    vfs: &Arc<dyn VirtualFs>,
    capability: &CapabilityToken,
    budget: &Arc<Mutex<ResourceBudget>>,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
) -> Result<Vec<u8>, SandboxError> {
    let _span = tracing::info_span!(
        "sandbox_read_file",
        simulacra.operation.name = "sandbox_read_file",
        simulacra.vfs.path = path,
    )
    .entered();

    // Memory paths are gated by MemoryCapability, not the generic paths_read glob —
    // attribute the denial counter accordingly so operators can filter memory
    // denials separately from generic-glob denials.
    check_and_journal_capability(
        || capability.check_path_read(path),
        "read_file",
        cap_name_for_read(path),
        journal,
        agent_id,
    )?;

    // Check global budget.
    {
        let b = budget
            .lock()
            .map_err(|e| SandboxError::Internal(format!("budget mutex poisoned: {e}")))?;
        if let Err(exhausted) = b.check_budget() {
            journal_budget_exhaustion(journal, agent_id, &exhausted);
            tracing::warn!(
                simulacra.budget.resource = %exhausted.resource,
                simulacra.budget.used = %exhausted.used,
                simulacra.budget.limit = %exhausted.limit,
                "budget exhausted"
            );
            return Err(SandboxError::BudgetExhausted(exhausted));
        }
    }

    // Execute.
    let data = match vfs.read(path) {
        Ok(data) => data,
        Err(err) => {
            if let Err(journal_err) = journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::ToolResult {
                    tool_call_id: None,
                    tool_name: "read_file".to_string(),
                    content: err.to_string(),
                    is_error: true,
                },
            }) {
                tracing::error!(error = %journal_err, "journal append failed for read_file error");
            }
            return Err(SandboxError::Vfs(err));
        }
    };

    // Journal the read.
    if let Err(err) = journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::ToolResult {
            tool_call_id: None,
            tool_name: "read_file".to_string(),
            content: format!("read {} bytes from {}", data.len(), path),
            is_error: false,
        },
    }) {
        tracing::error!(error = %err, "journal append failed for read_file");
    }

    Ok(data)
}

/// Core write_file logic: span → capability → budget → journal → execute → budget increment.
pub(crate) fn write_file_inner(
    path: &str,
    data: &[u8],
    vfs: &Arc<dyn VirtualFs>,
    capability: &CapabilityToken,
    budget: &Arc<Mutex<ResourceBudget>>,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
) -> Result<(), SandboxError> {
    let _span = tracing::info_span!(
        "sandbox_write_file",
        simulacra.operation.name = "sandbox_write_file",
        simulacra.vfs.path = path,
        simulacra.vfs.bytes = data.len() as u64,
    )
    .entered();

    check_and_journal_capability(
        || capability.check_path_write(path),
        "write_file",
        cap_name_for_write(path),
        journal,
        agent_id,
    )?;

    let write_bytes = data.len() as u64;
    reserve_vfs_bytes(budget, write_bytes, journal, agent_id)?;

    // Journal the write (before execution).
    if let Err(err) = journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::FileWrite {
            path: path.to_string(),
            size_bytes: data.len() as u64,
        },
    }) {
        tracing::error!(error = %err, "journal append failed for write_file");
    }

    // Execute. If the VFS rejects the write, roll back the byte reservation.
    if let Err(err) = vfs.write(path, data) {
        release_vfs_bytes(budget, write_bytes)?;
        return Err(SandboxError::Vfs(err));
    }

    Ok(())
}
