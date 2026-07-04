//! SQLite-backed journal storage implementation.
//!
//! Uses a single `journal_entries` table with auto-increment rowid.
//! Entries are stored as JSON blobs in the `entry_json` column.
//! Thread-safe via `Mutex<Connection>`.

use rusqlite::Connection;
use simulacra_types::{
    AgentId, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalError,
    JournalStorage, TokenUsage,
};
use std::path::PathBuf;
use std::sync::Mutex;

const JOURNAL_SCHEMA_SQL: &str = "PRAGMA journal_mode = WAL;
     PRAGMA synchronous = NORMAL;
     CREATE TABLE IF NOT EXISTS journal_entries (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         agent_id TEXT NOT NULL,
         schema_version INTEGER NOT NULL,
         timestamp_ms INTEGER NOT NULL,
         entry_json TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_journal_agent_id
         ON journal_entries (agent_id);";

/// SQLite-backed journal storage.
///
/// Each entry is stored as a row with the agent_id indexed for fast
/// per-agent queries. The entry payload is JSON-serialized.
#[derive(Debug)]
pub struct SqliteJournalStorage {
    conn: Mutex<Connection>,
}

impl SqliteJournalStorage {
    /// Create a new SQLite-backed journal at the given path.
    /// Creates the database and table if they don't exist.
    pub fn new(path: PathBuf) -> Result<Self, JournalError> {
        let conn = crate::sqlite_util::open_sqlite(&path, JOURNAL_SCHEMA_SQL)
            .map_err(JournalError::Storage)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create an in-memory SQLite journal (useful for testing).
    pub fn in_memory() -> Result<Self, JournalError> {
        let conn = crate::sqlite_util::open_in_memory_sqlite(
            "CREATE TABLE IF NOT EXISTS journal_entries (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 agent_id TEXT NOT NULL,
                 schema_version INTEGER NOT NULL,
                 timestamp_ms INTEGER NOT NULL,
                 entry_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_journal_agent_id
                 ON journal_entries (agent_id);",
        )
        .map_err(JournalError::Storage)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

impl JournalStorage for SqliteJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        let entry_json = serde_json::to_string(&entry.entry)
            .map_err(|e| JournalError::Storage(e.to_string()))?;

        let conn = crate::sqlite_util::lock_mutex(&self.conn).map_err(JournalError::Storage)?;

        conn.execute(
            "INSERT INTO journal_entries (agent_id, schema_version, timestamp_ms, entry_json)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                entry.agent_id.0,
                entry.schema_version,
                entry.timestamp_ms,
                entry_json,
            ],
        )
        .map_err(|e| JournalError::Storage(e.to_string()))?;

        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        let conn = crate::sqlite_util::lock_mutex(&self.conn).map_err(JournalError::Storage)?;

        let mut stmt = conn
            .prepare(
                "SELECT agent_id, schema_version, timestamp_ms, entry_json
                 FROM journal_entries
                 WHERE agent_id = ?1
                 ORDER BY id",
            )
            .map_err(|e| JournalError::Storage(e.to_string()))?;

        let entries = stmt
            .query_map(rusqlite::params![agent_id.0], |row| {
                let agent_id_str: String = row.get(0)?;
                let schema_version: u32 = row.get(1)?;
                let timestamp_ms: u64 = row.get(2)?;
                let entry_json: String = row.get(3)?;
                Ok((agent_id_str, schema_version, timestamp_ms, entry_json))
            })
            .map_err(|e| JournalError::Storage(e.to_string()))?;

        let mut result = Vec::new();
        for row in entries {
            let (agent_id_str, schema_version, timestamp_ms, entry_json) =
                row.map_err(|e| JournalError::Storage(e.to_string()))?;

            if schema_version > JOURNAL_SCHEMA_VERSION {
                tracing::error!(
                    "schema version mismatch: expected {} but found {}",
                    JOURNAL_SCHEMA_VERSION,
                    schema_version
                );
                return Err(JournalError::SchemaVersionMismatch {
                    expected: JOURNAL_SCHEMA_VERSION,
                    got: schema_version,
                });
            }

            let entry: JournalEntryKind = serde_json::from_str(&entry_json)
                .map_err(|e| JournalError::Storage(e.to_string()))?;

            result.push(JournalEntry {
                schema_version,
                agent_id: AgentId(agent_id_str),
                timestamp_ms,
                entry,
            });
        }
        Ok(result)
    }

    fn query_token_usage(&self, agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        let conn = crate::sqlite_util::lock_mutex(&self.conn).map_err(JournalError::Storage)?;

        let mut stmt = conn
            .prepare(
                "SELECT entry_json FROM journal_entries
                 WHERE agent_id = ?1
                 ORDER BY id",
            )
            .map_err(|e| JournalError::Storage(e.to_string()))?;

        let mut total = TokenUsage::default();
        let rows = stmt
            .query_map(rusqlite::params![agent_id.0], |row| {
                let entry_json: String = row.get(0)?;
                Ok(entry_json)
            })
            .map_err(|e| JournalError::Storage(e.to_string()))?;

        for row in rows {
            let entry_json = row.map_err(|e| JournalError::Storage(e.to_string()))?;
            if let Ok(JournalEntryKind::LlmResponse { token_usage, .. }) =
                serde_json::from_str::<JournalEntryKind>(&entry_json)
            {
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
        let conn = crate::sqlite_util::lock_mutex(&self.conn).map_err(JournalError::Storage)?;

        // Validate after_entry is within bounds
        let agent_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM journal_entries WHERE agent_id = ?1",
                rusqlite::params![agent_id.0],
                |row| row.get(0),
            )
            .map_err(|e| JournalError::Storage(e.to_string()))?;

        if after_entry > agent_count {
            return Err(JournalError::InvalidCheckpointIndex(after_entry));
        }

        let serialized =
            serde_json::to_vec(&data).map_err(|e| JournalError::Storage(e.to_string()))?;
        let entry = JournalEntryKind::Checkpoint {
            snapshot_data: serialized,
        };
        let entry_json =
            serde_json::to_string(&entry).map_err(|e| JournalError::Storage(e.to_string()))?;

        conn.execute(
            "INSERT INTO journal_entries (agent_id, schema_version, timestamp_ms, entry_json)
             VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![agent_id.0, JOURNAL_SCHEMA_VERSION, entry_json,],
        )
        .map_err(|e| JournalError::Storage(e.to_string()))?;

        Ok(())
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let all = self.read_all(agent_id)?;

        if checkpoint_idx >= all.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }

        if !matches!(
            all[checkpoint_idx].entry,
            JournalEntryKind::Checkpoint { .. }
        ) {
            return Err(JournalError::NotFound(format!(
                "entry at index {checkpoint_idx} is not a checkpoint"
            )));
        }

        Ok(all[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let all = self.read_all(agent_id)?;

        if start_index > all.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }

        Ok(all[start_index..].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use simulacra_types::{Message, ResourceBudget, Role};

    fn make_entry(agent_id: &str, kind: JournalEntryKind) -> JournalEntry {
        JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: AgentId(agent_id.into()),
            timestamp_ms: 1000,
            entry: kind,
        }
    }

    #[test]
    fn append_and_read_all_roundtrip() {
        let storage = SqliteJournalStorage::in_memory().unwrap();
        let agent = AgentId("test-agent".into());

        storage
            .append(make_entry("test-agent", JournalEntryKind::TurnStart))
            .unwrap();
        storage
            .append(make_entry(
                "test-agent",
                JournalEntryKind::LlmRequest {
                    model: "gpt-4".into(),
                    message_count: 3,
                },
            ))
            .unwrap();

        let entries = storage.read_all(&agent).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0].entry, JournalEntryKind::TurnStart));
        assert!(matches!(
            entries[1].entry,
            JournalEntryKind::LlmRequest { .. }
        ));
    }

    #[test]
    fn query_token_usage_sums_llm_responses() {
        let storage = SqliteJournalStorage::in_memory().unwrap();
        let agent = AgentId("token-agent".into());

        storage
            .append(make_entry(
                "token-agent",
                JournalEntryKind::LlmResponse {
                    model: "gpt-4".into(),
                    token_usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                    },
                    finish_reason: "EndTurn".into(),
                    assistant_message: None,
                },
            ))
            .unwrap();
        storage
            .append(make_entry(
                "token-agent",
                JournalEntryKind::LlmResponse {
                    model: "gpt-4".into(),
                    token_usage: TokenUsage {
                        input_tokens: 200,
                        output_tokens: 75,
                    },
                    finish_reason: "EndTurn".into(),
                    assistant_message: None,
                },
            ))
            .unwrap();

        let usage = storage.query_token_usage(&agent).unwrap();
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 125);
    }

    #[test]
    fn agents_are_isolated() {
        let storage = SqliteJournalStorage::in_memory().unwrap();

        storage
            .append(make_entry("agent-a", JournalEntryKind::TurnStart))
            .unwrap();
        storage
            .append(make_entry("agent-b", JournalEntryKind::TurnStart))
            .unwrap();
        storage
            .append(make_entry("agent-a", JournalEntryKind::TurnStart))
            .unwrap();

        let a_entries = storage.read_all(&AgentId("agent-a".into())).unwrap();
        let b_entries = storage.read_all(&AgentId("agent-b".into())).unwrap();
        assert_eq!(a_entries.len(), 2);
        assert_eq!(b_entries.len(), 1);
    }

    #[test]
    fn checkpoint_and_fork() {
        let storage = SqliteJournalStorage::in_memory().unwrap();
        let agent = AgentId("fork-agent".into());

        storage
            .append(make_entry("fork-agent", JournalEntryKind::TurnStart))
            .unwrap();
        storage
            .save_checkpoint(
                &agent,
                1,
                CheckpointData {
                    messages: vec![Message {
                        role: Role::Assistant,
                        content: "checkpoint".into(),
                        tool_calls: vec![],
                        tool_call_id: None,
                    }],
                    budget_snapshot: ResourceBudget::new(256, 8, Decimal::new(100, 0), 0),
                    vfs_snapshot: None,
                },
            )
            .unwrap();
        storage
            .append(make_entry(
                "fork-agent",
                JournalEntryKind::FileWrite {
                    path: "after-checkpoint.txt".into(),
                    size_bytes: 42,
                },
            ))
            .unwrap();

        let forked = storage.fork_from(&agent, 1).unwrap();
        assert_eq!(forked.len(), 2); // TurnStart + Checkpoint
        assert!(matches!(forked[0].entry, JournalEntryKind::TurnStart));
        assert!(matches!(
            forked[1].entry,
            JournalEntryKind::Checkpoint { .. }
        ));
    }

    #[test]
    fn read_from_returns_entries_after_index() {
        let storage = SqliteJournalStorage::in_memory().unwrap();
        let agent = AgentId("read-from-agent".into());

        for _ in 0..5 {
            storage
                .append(make_entry("read-from-agent", JournalEntryKind::TurnStart))
                .unwrap();
        }

        let from_2 = storage.read_from(&agent, 2).unwrap();
        assert_eq!(from_2.len(), 3);
    }

    #[test]
    fn schema_version_mismatch_rejected() {
        let storage = SqliteJournalStorage::in_memory().unwrap();
        let agent = AgentId("schema-agent".into());

        storage
            .append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION + 1,
                agent_id: agent.clone(),
                timestamp_ms: 1,
                entry: JournalEntryKind::TurnStart,
            })
            .unwrap();

        let err = storage
            .read_all(&agent)
            .expect_err("should reject future schema version");
        assert!(matches!(err, JournalError::SchemaVersionMismatch { .. }));
    }

    #[test]
    fn file_backed_persistence() {
        let dir =
            std::env::temp_dir().join(format!("simulacra-journal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.db");

        // Write some entries
        {
            let storage = SqliteJournalStorage::new(db_path.clone()).unwrap();
            storage
                .append(make_entry("persist-agent", JournalEntryKind::TurnStart))
                .unwrap();
            storage
                .append(make_entry(
                    "persist-agent",
                    JournalEntryKind::FileWrite {
                        path: "hello.txt".into(),
                        size_bytes: 5,
                    },
                ))
                .unwrap();
        }

        // Re-open and verify
        {
            let storage = SqliteJournalStorage::new(db_path.clone()).unwrap();
            let entries = storage.read_all(&AgentId("persist-agent".into())).unwrap();
            assert_eq!(entries.len(), 2);
            assert!(matches!(entries[0].entry, JournalEntryKind::TurnStart));
            assert!(matches!(
                entries[1].entry,
                JournalEntryKind::FileWrite { .. }
            ));
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
