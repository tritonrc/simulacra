use simulacra_types::{
    AgentId, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalStorage,
};

use super::FetchError;

/// Append a single `JournalEntryKind::HttpRequest` entry. Called AFTER
/// the dispatch path completes so the entry's `status` differentiates
/// success (the upstream HTTP code) from denial / hook-block / transport
/// error / timeout (`status = 0`). The dispatch's full outcome continues
/// to be visible on the `simulacra_mcp_http_fetch` span for o11y consumers.
/// Append failures bubble up as `FetchError::Transport`; callers must fail
/// closed so a side-effecting fetch never returns to the module without a
/// durable audit entry.
pub(crate) fn journal_fetch(
    journal: Option<&dyn JournalStorage>,
    agent_id: &AgentId,
    method: &str,
    url: &str,
    status: u16,
) -> Result<(), FetchError> {
    let Some(journal) = journal else {
        return Ok(());
    };
    let entry = JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        entry: JournalEntryKind::HttpRequest {
            method: method.to_string(),
            url: url.to_string(),
            status,
        },
    };
    journal.append(entry).map_err(|err| {
        tracing::warn!(
            error = %err,
            method = method,
            url = url,
            "wasm_mcp_fetch journal append failed"
        );
        FetchError::Transport(format!("journal append failed: {err}"))
    })
}
