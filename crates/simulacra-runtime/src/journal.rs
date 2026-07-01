//! In-memory journal storage implementation.

use simulacra_types::{
    AgentId, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalError,
    JournalStorage, TokenUsage,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// In-memory journal storage backed by a `RwLock<Vec<JournalEntry>>`.
#[derive(Debug, Default)]
pub struct InMemoryJournalStorage {
    entries: RwLock<Vec<JournalEntry>>,
}

impl InMemoryJournalStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Validate schema version of a journal entry.
/// Returns an error if the entry's schema version exceeds the current version.
fn validate_schema_version(entry: &JournalEntry) -> Result<(), JournalError> {
    if entry.schema_version > JOURNAL_SCHEMA_VERSION {
        tracing::error!(
            "schema version mismatch: expected {} but found {}",
            JOURNAL_SCHEMA_VERSION,
            entry.schema_version
        );
        return Err(JournalError::SchemaVersionMismatch {
            expected: JOURNAL_SCHEMA_VERSION,
            got: entry.schema_version,
        });
    }
    Ok(())
}

impl JournalStorage for InMemoryJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        let mut entries = self
            .entries
            .write()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        entries.push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let mut result = Vec::new();
        for entry in entries.iter().filter(|e| e.agent_id == *agent_id) {
            validate_schema_version(entry)?;
            result.push(entry.clone());
        }
        Ok(result)
    }

    fn query_token_usage(&self, agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let mut total = TokenUsage::default();
        for entry in entries.iter().filter(|e| e.agent_id == *agent_id) {
            if let JournalEntryKind::LlmResponse { token_usage, .. } = &entry.entry {
                total.input_tokens += token_usage.input_tokens;
                total.output_tokens += token_usage.output_tokens;
            }
        }
        Ok(total)
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let serialized =
            serde_json::to_vec(&data).map_err(|e| JournalError::Storage(e.to_string()))?;
        let entry = JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0, // Checkpoints don't need wall-clock time
            entry: JournalEntryKind::Checkpoint {
                snapshot_data: serialized,
            },
        };
        // Validate that after_entry is within bounds
        let mut entries = self
            .entries
            .write()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let agent_count = entries.iter().filter(|e| e.agent_id == *agent_id).count();
        if after_entry > agent_count {
            return Err(JournalError::InvalidCheckpointIndex(after_entry));
        }
        entries.push(entry);
        Ok(())
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let agent_entries: Vec<JournalEntry> = entries
            .iter()
            .filter(|e| e.agent_id == *agent_id)
            .cloned()
            .collect();

        if checkpoint_idx >= agent_entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }

        // The entry at checkpoint_idx must be a Checkpoint
        if !matches!(
            agent_entries[checkpoint_idx].entry,
            JournalEntryKind::Checkpoint { .. }
        ) {
            return Err(JournalError::NotFound(format!(
                "entry at index {checkpoint_idx} is not a checkpoint"
            )));
        }

        // Return entries up to and including the checkpoint
        Ok(agent_entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| JournalError::Storage(format!("lock poisoned: {e}")))?;
        let agent_entries: Vec<JournalEntry> = entries
            .iter()
            .filter(|e| e.agent_id == *agent_id)
            .cloned()
            .collect();

        if start_index > agent_entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }

        // Validate schema version on each entry (same as read_all)
        for entry in &agent_entries[start_index..] {
            if entry.schema_version != JOURNAL_SCHEMA_VERSION {
                return Err(JournalError::SchemaVersionMismatch {
                    expected: JOURNAL_SCHEMA_VERSION,
                    got: entry.schema_version,
                });
            }
        }

        Ok(agent_entries[start_index..].to_vec())
    }
}

/// Journal wrapper that mirrors successful appends into an atomic counter.
///
/// This is used by `/proc/session/journal_entries` so agent-visible
/// orientation reflects all journal writes, including writes performed by
/// sandbox/tool layers outside the `AgentLoop`.
pub struct CountingJournalStorage {
    inner: Arc<dyn JournalStorage>,
    counter: Arc<AtomicU64>,
}

impl CountingJournalStorage {
    pub fn new(inner: Arc<dyn JournalStorage>, counter: Arc<AtomicU64>) -> Self {
        Self { inner, counter }
    }
}

impl JournalStorage for CountingJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.inner.append(entry)?;
        self.counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.read_all(agent_id)
    }

    fn query_token_usage(&self, agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        self.inner.query_token_usage(agent_id)
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        self.inner.save_checkpoint(agent_id, after_entry, data)?;
        self.counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.fork_from(agent_id, checkpoint_idx)
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.read_from(agent_id, start_index)
    }
}

#[cfg(all(test, feature = "spawn"))]
mod tests;
