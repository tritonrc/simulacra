use crate::error::McpError;
use simulacra_types::{JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind};

use super::McpManager;

impl McpManager {
    /// Append a Journal ToolCall entry if a journal storage backend is configured.
    ///
    /// Returns an error if the journal append fails. The caller MUST NOT proceed
    /// with the side effect (MCP dispatch) when this returns Err — a missing
    /// journal entry makes replay non-deterministic. See the "Journal Before
    /// Return" invariant in ARCHITECTURE.md.
    pub(crate) fn append_journal_tool_call(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(), McpError> {
        if let Some(ref journal) = self.journal {
            let entry = JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                entry: JournalEntryKind::ToolCall {
                    tool_call_id: None,
                    tool_name: tool_name.to_string(),
                    arguments: arguments.clone(),
                },
            };
            journal.append(entry).map_err(|e| {
                tracing::warn!(
                    error = %e,
                    tool = tool_name,
                    "journal append failed — aborting MCP dispatch to preserve replay determinism"
                );
                McpError::ProtocolError(format!("journal append failed: {e}"))
            })?;
        }
        Ok(())
    }
}
