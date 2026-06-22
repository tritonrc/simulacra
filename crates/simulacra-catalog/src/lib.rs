//! S042 — Agent Catalog. SQLite-backed (default) or in-memory (`--no-catalog`).

pub mod agent_file_fs;
pub mod error;
pub mod ids;
pub mod migrate;
pub mod models;
pub mod repo;
pub mod skill_fs;

pub use agent_file_fs::CatalogAgentFileFs;
pub use error::CatalogError;
pub use ids::{AgentFileId, AgentId, ChannelId, MemoryPoolId, SkillId, TenantId};
pub use models::{
    Agent, AgentFile, AgentPatch, Channel, ChannelKind, ChannelPatch, MemoryPool, MemoryPoolPatch,
    NewAgent, NewAgentFile, NewChannel, NewMemoryPool, NewSkill, Page, PageRequest, ResolvedAgent,
    Skill, SkillPatch, Tenant,
};
pub use repo::sqlite::{AgentFileStore, SqliteBlobAgentFileStore};
pub use skill_fs::CatalogSkillFs;

use chrono::Utc;
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Top-level catalog handle. Owns the connection; produces repository handles
/// via `agents()`, `skills()`, etc.
pub struct Catalog {
    conn: Arc<Mutex<Connection>>,
    /// S045 — byte storage backend for per-agent files. Defaults to
    /// [`SqliteBlobAgentFileStore`] over the catalog connection; tests and
    /// future filesystem/S3 backends inject a custom impl via
    /// [`Catalog::open_in_memory_with_agent_file_store`] (or the future
    /// path-mode equivalent).
    agent_file_store: Arc<dyn AgentFileStore>,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        configure_pragmas(&conn)?;
        migrate::run(&mut conn)?;
        let conn = Arc::new(Mutex::new(conn));
        let agent_file_store: Arc<dyn AgentFileStore> =
            Arc::new(SqliteBlobAgentFileStore::new(Arc::clone(&conn)));
        Ok(Self {
            conn,
            agent_file_store,
        })
    }

    pub fn open_in_memory() -> Result<Self, CatalogError> {
        let mut conn = Connection::open_in_memory()?;
        configure_pragmas(&conn)?;
        migrate::run(&mut conn)?;
        let conn = Arc::new(Mutex::new(conn));
        let agent_file_store: Arc<dyn AgentFileStore> =
            Arc::new(SqliteBlobAgentFileStore::new(Arc::clone(&conn)));
        Ok(Self {
            conn,
            agent_file_store,
        })
    }

    /// Open an in-memory catalog with a caller-provided
    /// [`AgentFileStore`]. Used by tests that want to assert against a
    /// recording fake, and by future filesystem/S3 backends that wire
    /// their own store at boot.
    pub fn open_in_memory_with_agent_file_store(
        agent_file_store: Arc<dyn AgentFileStore>,
    ) -> Result<Self, CatalogError> {
        let mut conn = Connection::open_in_memory()?;
        configure_pragmas(&conn)?;
        migrate::run(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            agent_file_store,
        })
    }

    pub(crate) fn conn(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }

    pub(crate) fn agent_file_store_arc(&self) -> Arc<dyn AgentFileStore> {
        Arc::clone(&self.agent_file_store)
    }

    /// Return whether the seed labelled `source` has been applied previously.
    ///
    /// Reads from the `seeds_applied` table populated by
    /// [`Catalog::mark_seed_applied`]. Used by `simulacra-cli`'s bootstrap import
    /// to gate one-shot TOML→DB seeding (S042 §"simulacra-cli bootstrap import").
    pub async fn is_seed_applied(&self, source: &str) -> Result<bool, CatalogError> {
        use rusqlite::OptionalExtension;
        let conn = self.conn();
        let source = source.to_owned();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().expect("catalog mutex poisoned");
            let exists: Option<String> = guard
                .query_row(
                    "SELECT source FROM seeds_applied WHERE source = ?1",
                    params![&source],
                    |row| row.get(0),
                )
                .optional()?;
            Ok::<bool, CatalogError>(exists.is_some())
        })
        .await?
    }

    /// Record that the seed labelled `source` has been applied. Idempotent —
    /// repeated calls on the same `source` are a no-op (the existing row is
    /// preserved; `applied_at` is not overwritten).
    pub async fn mark_seed_applied(&self, source: &str) -> Result<(), CatalogError> {
        let conn = self.conn();
        let source = source.to_owned();
        let now = Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().expect("catalog mutex poisoned");
            guard.execute(
                "INSERT OR IGNORE INTO seeds_applied (source, applied_at) VALUES (?1, ?2)",
                params![&source, &now],
            )?;
            Ok::<(), CatalogError>(())
        })
        .await?
    }
}

fn configure_pragmas(conn: &Connection) -> Result<(), CatalogError> {
    // journal_mode=WAL, synchronous=NORMAL, foreign_keys=ON, busy_timeout=10000ms.
    // WAL is rejected silently by an in-memory DB but `pragma_update` still
    // succeeds; the test suite only asserts WAL on a file-backed DB.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_millis(10_000))?;
    Ok(())
}

#[cfg(feature = "test-internals")]
impl Catalog {
    pub fn conn_for_tests(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.conn)
    }
}
