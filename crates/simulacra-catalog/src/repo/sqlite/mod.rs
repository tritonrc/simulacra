use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD as B64};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

use crate::{Catalog, CatalogError};

pub mod agent;
pub mod agent_file;
pub mod channel;
pub mod memory_pool;
pub mod skill;
pub mod tenant;

pub use agent::SqliteAgentRepository;
pub use agent_file::{AgentFileStore, SqliteAgentFileRepository, SqliteBlobAgentFileStore};
pub use channel::SqliteChannelRepository;
pub use memory_pool::SqliteMemoryPoolRepository;
pub use skill::SqliteSkillRepository;
pub use tenant::SqliteTenantRepository;

pub(crate) type SharedConn = Arc<Mutex<Connection>>;

impl Catalog {
    pub fn tenants(&self) -> SqliteTenantRepository {
        SqliteTenantRepository::new(self.conn())
    }

    pub fn agents(&self) -> SqliteAgentRepository {
        SqliteAgentRepository::new(self.conn())
    }

    pub fn skills(&self) -> SqliteSkillRepository {
        SqliteSkillRepository::new(self.conn())
    }

    pub fn memory_pools(&self) -> SqliteMemoryPoolRepository {
        SqliteMemoryPoolRepository::new(self.conn())
    }

    pub fn agent_files(&self) -> SqliteAgentFileRepository {
        SqliteAgentFileRepository::new(self.conn(), self.agent_file_store_arc())
    }

    pub fn channels(&self) -> SqliteChannelRepository {
        SqliteChannelRepository::new(self.conn())
    }

    pub fn agent_file_store(&self) -> Arc<dyn AgentFileStore> {
        self.agent_file_store_arc()
    }
}

pub(crate) async fn blocking<F, R>(conn: SharedConn, f: F) -> Result<R, CatalogError>
where
    F: FnOnce(&mut Connection) -> Result<R, CatalogError> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = conn.lock().expect("catalog mutex poisoned");
        f(&mut guard)
    })
    .await?
}

/// Parse an RFC3339 timestamp from a SQL TEXT column into `DateTime<Utc>`.
pub(crate) fn parse_ts(s: String) -> rusqlite::Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

/// Encode a (`created_at`, `id`) pagination cursor as base64-encoded
/// `created_at|id`. Sort key is (created_at ASC, id ASC).
pub(crate) fn encode_cursor(created_at: &str, id: &str) -> String {
    B64.encode(format!("{created_at}|{id}"))
}

pub(crate) fn decode_cursor(cursor: &str) -> Result<(String, String), CatalogError> {
    let bytes = B64
        .decode(cursor)
        .map_err(|_| CatalogError::Validation("invalid cursor".into()))?;
    let s =
        String::from_utf8(bytes).map_err(|_| CatalogError::Validation("invalid cursor".into()))?;
    let mut parts = s.splitn(2, '|');
    let ts = parts
        .next()
        .ok_or_else(|| CatalogError::Validation("invalid cursor".into()))?
        .to_owned();
    let id = parts
        .next()
        .ok_or_else(|| CatalogError::Validation("invalid cursor".into()))?
        .to_owned();
    Ok((ts, id))
}
