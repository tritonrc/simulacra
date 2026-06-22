//! SQLite-backed session storage implementation.
//!
//! Stores sessions in a `sessions` table with the session data as JSON.
//! Thread-safe via `Mutex<Connection>`.

use crate::{RuntimeError, Session, SessionStorage};
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::Mutex;

/// SQLite-backed session storage.
///
/// Sessions are stored as JSON blobs keyed by session id.
/// Shares the same database file as `SqliteJournalStorage` when
/// both are pointed at the same path.
#[derive(Debug)]
pub struct SqliteSessionStorage {
    conn: Mutex<Connection>,
}

impl SqliteSessionStorage {
    /// Create a new SQLite-backed session store at the given path.
    /// Creates the database and table if they don't exist.
    pub fn new(path: PathBuf) -> Result<Self, RuntimeError> {
        let conn = Connection::open(&path).map_err(|e| RuntimeError::Session(e.to_string()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 agent_id TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 session_json TEXT NOT NULL
             );",
        )
        .map_err(|e| RuntimeError::Session(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create an in-memory SQLite session store (useful for testing).
    pub fn in_memory() -> Result<Self, RuntimeError> {
        let conn =
            Connection::open_in_memory().map_err(|e| RuntimeError::Session(e.to_string()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 agent_id TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 session_json TEXT NOT NULL
             );",
        )
        .map_err(|e| RuntimeError::Session(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

impl SessionStorage for SqliteSessionStorage {
    fn save(&self, session: &Session) -> Result<(), RuntimeError> {
        let session_json =
            serde_json::to_string(session).map_err(|e| RuntimeError::Session(e.to_string()))?;

        let conn = self
            .conn
            .lock()
            .map_err(|e| RuntimeError::Session(format!("lock poisoned: {e}")))?;

        conn.execute(
            "INSERT OR REPLACE INTO sessions (id, agent_id, created_at, session_json)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                session.id,
                session.agent_id.0,
                session.created_at,
                session_json,
            ],
        )
        .map_err(|e| RuntimeError::Session(e.to_string()))?;

        Ok(())
    }

    fn load(&self, id: &str) -> Result<Option<Session>, RuntimeError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| RuntimeError::Session(format!("lock poisoned: {e}")))?;

        let mut stmt = conn
            .prepare("SELECT session_json FROM sessions WHERE id = ?1")
            .map_err(|e| RuntimeError::Session(e.to_string()))?;

        let result = stmt
            .query_row(rusqlite::params![id], |row| {
                let json: String = row.get(0)?;
                Ok(json)
            })
            .optional()
            .map_err(|e| RuntimeError::Session(e.to_string()))?;

        match result {
            Some(json) => {
                let session: Session = serde_json::from_str(&json)
                    .map_err(|e| RuntimeError::Session(e.to_string()))?;
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulacra_types::{AgentId, Message, Role};

    fn make_session(id: &str) -> Session {
        Session {
            id: id.into(),
            agent_id: AgentId("test-agent".into()),
            messages: vec![Message {
                role: Role::User,
                content: "hello".into(),
                tool_calls: vec![],
                tool_call_id: None,
            }],
            vfs_snapshot: None,
            created_at: 1000,
            used_tokens: 0,
            used_turns: 0,
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let storage = SqliteSessionStorage::in_memory().unwrap();
        let session = make_session("sess-1");

        storage.save(&session).unwrap();
        let loaded = storage
            .load("sess-1")
            .unwrap()
            .expect("session should exist");

        assert_eq!(loaded.id, "sess-1");
        assert_eq!(loaded.agent_id.0, "test-agent");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "hello");
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let storage = SqliteSessionStorage::in_memory().unwrap();
        assert!(storage.load("nonexistent").unwrap().is_none());
    }

    #[test]
    fn save_overwrites_existing() {
        let storage = SqliteSessionStorage::in_memory().unwrap();
        let mut session = make_session("sess-2");
        storage.save(&session).unwrap();

        session.messages.push(Message {
            role: Role::Assistant,
            content: "updated".into(),
            tool_calls: vec![],
            tool_call_id: None,
        });
        storage.save(&session).unwrap();

        let loaded = storage.load("sess-2").unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[1].content, "updated");
    }

    #[test]
    fn sessions_are_isolated_by_id() {
        let storage = SqliteSessionStorage::in_memory().unwrap();
        storage.save(&make_session("a")).unwrap();
        storage.save(&make_session("b")).unwrap();

        let a = storage.load("a").unwrap().unwrap();
        let b = storage.load("b").unwrap().unwrap();
        assert_eq!(a.id, "a");
        assert_eq!(b.id, "b");
    }

    #[test]
    fn file_backed_persistence() {
        let dir =
            std::env::temp_dir().join(format!("simulacra-session-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("sessions.db");

        {
            let storage = SqliteSessionStorage::new(db_path.clone()).unwrap();
            storage.save(&make_session("persist-1")).unwrap();
        }

        {
            let storage = SqliteSessionStorage::new(db_path.clone()).unwrap();
            let loaded = storage.load("persist-1").unwrap().expect("should persist");
            assert_eq!(loaded.id, "persist-1");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
