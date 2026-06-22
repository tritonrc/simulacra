//! Session management for agent conversations.

use crate::RuntimeError;
use simulacra_types::{AgentId, Message, VfsSnapshot};
use std::collections::HashMap;
use std::sync::RwLock;

/// A conversation session bound to a single agent.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub id: String,
    pub agent_id: AgentId,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vfs_snapshot: Option<VfsSnapshot>,
    pub created_at: u64,
    /// Total tokens consumed so far in this session. Persisted across
    /// checkpoints so that resuming an existing session preserves the
    /// budget snapshot rather than resetting usage to zero.
    #[serde(default)]
    pub used_tokens: u64,
    /// Total agent turns consumed so far in this session. Same rationale
    /// as `used_tokens`.
    #[serde(default)]
    pub used_turns: u32,
}

/// Storage backend for sessions. Object-safe.
pub trait SessionStorage: Send + Sync + 'static {
    fn save(&self, session: &Session) -> Result<(), RuntimeError>;
    fn load(&self, id: &str) -> Result<Option<Session>, RuntimeError>;
}

/// In-memory session storage backed by a `RwLock<HashMap>`.
#[derive(Debug, Default)]
pub struct InMemorySessionStorage {
    store: RwLock<HashMap<String, Session>>,
}

impl InMemorySessionStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStorage for InMemorySessionStorage {
    fn save(&self, session: &Session) -> Result<(), RuntimeError> {
        let mut store = self
            .store
            .write()
            .map_err(|e| RuntimeError::Session(format!("lock poisoned: {e}")))?;
        store.insert(session.id.clone(), session.clone());
        Ok(())
    }

    fn load(&self, id: &str) -> Result<Option<Session>, RuntimeError> {
        let store = self
            .store
            .read()
            .map_err(|e| RuntimeError::Session(format!("lock poisoned: {e}")))?;
        Ok(store.get(id).cloned())
    }
}
