//! Journal entries for the VFS mutation batch: the plan, per-path effects, and
//! a failed execution.

use simulacra_types::{JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, VfsError};

use crate::{AgentCell, SandboxError};

pub(crate) fn journal_plan(
    cell: &AgentCell,
    tool_name: &str,
    count: usize,
) -> Result<(), SandboxError> {
    match cell.journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: cell.agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::ToolResult {
            tool_call_id: None,

            tool_name: tool_name.to_string(),
            content: format!("planned {} VFS mutation(s)", count),
            is_error: false,
            provider_content: Vec::new(),
        },
    }) {
        Ok(()) => Ok(()),
        Err(err) => {
            tracing::error!(error = %err, "journal append failed for VFS mutation plan");
            Err(SandboxError::Internal(format!(
                "journal append failed for VFS mutation plan: {err}"
            )))
        }
    }
}

pub(crate) fn journal_deletes(cell: &AgentCell, paths: &[&str]) -> Result<(), SandboxError> {
    for path in paths {
        if let Err(err) = cell.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: cell.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::FileDelete {
                path: path.to_string(),
            },
        }) {
            tracing::error!(error = %err, "journal append failed for VFS mutation delete");
            return Err(SandboxError::Internal(format!(
                "journal append failed for VFS mutation delete: {err}"
            )));
        }
    }
    Ok(())
}

pub(crate) fn journal_moves(cell: &AgentCell, moves: &[(&str, &str)]) -> Result<(), SandboxError> {
    for (from, to) in moves {
        if let Err(err) = cell.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: cell.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::FileMove {
                from: from.to_string(),
                to: to.to_string(),
            },
        }) {
            tracing::error!(error = %err, "journal append failed for VFS mutation move");
            return Err(SandboxError::Internal(format!(
                "journal append failed for VFS mutation move: {err}"
            )));
        }
    }
    Ok(())
}

pub(crate) fn journal_writes(cell: &AgentCell, writes: &[(&str, u64)]) -> Result<(), SandboxError> {
    for (path, size_bytes) in writes {
        if let Err(err) = cell.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: cell.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::FileWrite {
                path: path.to_string(),
                size_bytes: *size_bytes,
            },
        }) {
            tracing::error!(error = %err, "journal append failed for VFS mutation write");
            return Err(SandboxError::Internal(format!(
                "journal append failed for VFS mutation write: {err}"
            )));
        }
    }
    Ok(())
}

/// Journal a batch whose outcome is unknown: it may have been applied.
pub(crate) fn journal_unknown_outcome(cell: &AgentCell, tool_name: &str, err: &VfsError) {
    journal_tool_error(
        cell,
        tool_name,
        format!("VFS mutation batch may have been applied: {err}"),
    );
}

pub(crate) fn journal_execution_failure(cell: &AgentCell, tool_name: &str, err: &VfsError) {
    journal_tool_error(cell, tool_name, err.to_string());
}

fn journal_tool_error(cell: &AgentCell, tool_name: &str, content: String) {
    if let Err(journal_err) = cell.journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: cell.agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::ToolResult {
            tool_call_id: None,

            tool_name: tool_name.to_string(),
            content,
            is_error: true,
            provider_content: Vec::new(),
        },
    }) {
        tracing::error!(error = %journal_err, "journal append failed for VFS mutation error");
    }
}
