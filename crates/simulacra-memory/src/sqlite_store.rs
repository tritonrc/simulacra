//! `SqliteMemoryStore` — concrete `MemoryStore` backed by per-tenant SQLite
//! databases.
//!
//! Per S037 §6, each tenant has its own SQLite file at
//! `{root}/memory/{tenant}.db`. This store owns the `memory_content` table.
//! The parallel `SqliteVectorIndex` owns `memory_chunks`, `memory_vectors`,
//! `memory_embed_backlog`, `memory_embedder_log`, AND `memory_schema_meta`
//! (the schema-meta row contains dim + embedder_id, which is the index's
//! concern, not the store's). Both implementations use `CREATE TABLE IF NOT
//! EXISTS` so they coexist on the same database file.
//!
//! **Event stream invariant:** multiple `SqliteMemoryStore` instances pointing
//! at the same root directory share a single broadcast sender via a
//! process-global registry. This guarantees that writes made through one
//! instance are visible to subscribers on another instance in the same
//! process. Cross-process event delivery is NOT supported — multi-process
//! deployments must use a single `SqliteMemoryStore` in a dedicated worker
//! process (see S037 §7 cross-process caveat).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use simulacra_types::{MemoryPath, MemoryVersion, TenantId};
use tokio::sync::broadcast;
use tracing::warn;

use crate::error::MemoryError;
use crate::store::{MemoryEntry, MemoryEvent, MemoryEventReceiver, MemoryRecvOutcome, MemoryStore};

/// Capacity of the broadcast channel used to publish `MemoryEvent`s. The
/// background embedder is the primary subscriber; a generous buffer keeps
/// us from dropping events under burst load.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Busy timeout on every connection. Spec floor is 5000 ms; the concurrency
/// tests hold external locks for slightly over 5 s, so 10 s gives headroom.
const BUSY_TIMEOUT_MS: u64 = 10_000;

/// Process-global registry of broadcast senders, keyed on the canonical
/// root path. Multiple `SqliteMemoryStore` instances at the same root share
/// one sender, so events from store A are visible to subscribers on store B.
static SENDER_REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<broadcast::Sender<MemoryEvent>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn sender_for_root(root: &Path) -> Arc<broadcast::Sender<MemoryEvent>> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut registry = SENDER_REGISTRY.lock().expect("sender registry poisoned");
    if let Some(weak) = registry.get(&canonical)
        && let Some(strong) = weak.upgrade()
    {
        return strong;
    }
    let (sender, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
    let arc = Arc::new(sender);
    registry.insert(canonical, Arc::downgrade(&arc));
    arc
}

/// Per-tenant write lock + DB-creation guard. Wrapped in `Arc` so callers can
/// hold a clone independent of the outer `HashMap` lock.
type TenantLock = Arc<Mutex<()>>;

pub struct SqliteMemoryStore {
    root: PathBuf,
    sender: Arc<broadcast::Sender<MemoryEvent>>,
    locks: Mutex<HashMap<TenantId, TenantLock>>,
}

impl SqliteMemoryStore {
    /// Open (or create) a memory store rooted at `root`. The
    /// `{root}/memory` directory is created if it does not exist. Per-tenant
    /// `.db` files are created lazily on first write to that tenant.
    ///
    /// Multiple `SqliteMemoryStore` instances at the same root directory
    /// share a single broadcast sender via a process-global registry, so
    /// events published by one are visible to subscribers on another.
    pub fn new(root: &Path) -> Result<Self, MemoryError> {
        let memory_dir = root.join("memory");
        std::fs::create_dir_all(&memory_dir)?;
        let sender = sender_for_root(root);
        Ok(Self {
            root: root.to_path_buf(),
            sender,
            locks: Mutex::new(HashMap::new()),
        })
    }

    fn db_path(&self, tenant: &TenantId) -> PathBuf {
        self.root
            .join("memory")
            .join(format!("{}.db", tenant.as_fs_segment()))
    }

    fn tenant_lock(&self, tenant: &TenantId) -> TenantLock {
        let mut locks = self.locks.lock().expect("tenant lock map poisoned");
        locks
            .entry(tenant.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Open a fresh connection to the per-tenant DB and apply the standard
    /// pragmas. Creates the schema if missing.
    fn open_conn(&self, tenant: &TenantId) -> Result<Connection, MemoryError> {
        let path = self.db_path(tenant);
        let conn = Connection::open(&path)?;
        // Pragmas per S037 §6.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        ensure_schema(&conn)?;
        Ok(conn)
    }
}

fn ensure_schema(conn: &Connection) -> Result<(), MemoryError> {
    // Only the tables this store owns. `memory_schema_meta` and the vector
    // tables are owned by `SqliteVectorIndex`; both implementations use
    // `IF NOT EXISTS` so they coexist on the same DB file.
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS memory_content (
            path            TEXT PRIMARY KEY,
            version         INTEGER NOT NULL,
            content_hash    BLOB NOT NULL,
            size            INTEGER NOT NULL,
            mtime_ns        INTEGER NOT NULL,
            data            BLOB NOT NULL,
            deleted         INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS memory_content_prefix
            ON memory_content(path);
        "#,
    )?;
    Ok(())
}

/// Compute the exclusive upper bound for a prefix range scan. Given a prefix
/// `p`, returns a string `u` such that all paths in `[p, u)` are exactly the
/// paths that start with `p`. This avoids SQL `LIKE` altogether, which is
/// both safer (no wildcard escaping concerns) and faster (range scan on the
/// primary key, not a full table scan).
///
/// The trick: append U+10FFFF (max Unicode scalar) to the prefix. No valid
/// UTF-8 path can contain this character (it would fail `MemoryPath::parse`),
/// so anything "greater than" the prefix but not under it sorts AFTER this
/// upper bound.
fn prefix_upper_bound(prefix: &str) -> String {
    let mut upper = String::with_capacity(prefix.len() + 4);
    upper.push_str(prefix);
    upper.push('\u{10FFFF}');
    upper
}

fn now_ns() -> i64 {
    // System clock before UNIX_EPOCH is a hard environment failure —
    // fail loud rather than silently emitting 1970 timestamps.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before UNIX_EPOCH")
        .as_nanos() as i64
}

fn ns_to_system_time(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + std::time::Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

impl MemoryStore for SqliteMemoryStore {
    fn put(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        data: &[u8],
    ) -> Result<MemoryVersion, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");

        let mut conn = self.open_conn(tenant)?;
        // BEGIN IMMEDIATE acquires the write lock upfront. This is critical
        // under WAL mode: a deferred transaction that upgrades from reader
        // to writer mid-stream returns BUSY immediately (to avoid deadlock)
        // and busy_timeout does NOT retry. IMMEDIATE lets busy_timeout do
        // its job.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let current: Option<i64> = tx
            .query_row(
                "SELECT version FROM memory_content WHERE path = ?1",
                params![path.as_str()],
                |row| row.get(0),
            )
            .optional()?;

        let new_version = current.unwrap_or(0) + 1;
        let hash = sha256(data);
        let mtime = now_ns();
        let size = data.len() as i64;

        tx.execute(
            r#"
            INSERT INTO memory_content (path, version, content_hash, size, mtime_ns, data, deleted)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
            ON CONFLICT(path) DO UPDATE SET
                version      = excluded.version,
                content_hash = excluded.content_hash,
                size         = excluded.size,
                mtime_ns     = excluded.mtime_ns,
                data         = excluded.data,
                deleted      = 0
            "#,
            params![
                path.as_str(),
                new_version,
                hash.as_slice(),
                size,
                mtime,
                data
            ],
        )?;

        tx.commit()?;

        let version = MemoryVersion(new_version as u64);
        // Best-effort broadcast: a missing receiver is not an error.
        let _ = self.sender.send(MemoryEvent::Put {
            tenant: tenant.clone(),
            path: path.clone(),
            version,
            content_hash: hash,
            bytes_len: data.len() as u64,
            produced_at: SystemTime::now(),
        });

        Ok(version)
    }

    fn get(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
    ) -> Result<(Vec<u8>, MemoryVersion), MemoryError> {
        let conn = self.open_conn(tenant)?;
        let row: Option<(Vec<u8>, i64, i64)> = conn
            .query_row(
                "SELECT data, version, deleted FROM memory_content WHERE path = ?1",
                params![path.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        match row {
            Some((data, version, 0)) => Ok((data, MemoryVersion(version as u64))),
            _ => Err(MemoryError::NotFound(path.as_str().to_string())),
        }
    }

    fn exists(&self, tenant: &TenantId, path: &MemoryPath) -> Result<bool, MemoryError> {
        let conn = self.open_conn(tenant)?;
        let deleted: Option<i64> = conn
            .query_row(
                "SELECT deleted FROM memory_content WHERE path = ?1",
                params![path.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(matches!(deleted, Some(0)))
    }

    fn list_prefix(
        &self,
        tenant: &TenantId,
        prefix: &MemoryPath,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let conn = self.open_conn(tenant)?;
        // Range scan instead of `LIKE %` — avoids wildcard escaping concerns
        // and uses the primary key index directly. `ORDER BY path` makes the
        // result deterministic so callers (MemoryStoreFs::list_dir, retention
        // reaper) get stable ordering.
        let prefix_str = prefix.as_str();
        let upper = prefix_upper_bound(prefix_str);
        let mut stmt = conn.prepare(
            "SELECT path, version, content_hash, size, mtime_ns
             FROM memory_content
             WHERE path >= ?1 AND path < ?2 AND deleted = 0
             ORDER BY path ASC",
        )?;
        let rows = stmt.query_map(params![prefix_str, upper], |row| {
            let path_str: String = row.get(0)?;
            let version: i64 = row.get(1)?;
            let hash: Vec<u8> = row.get(2)?;
            let size: i64 = row.get(3)?;
            let mtime_ns: i64 = row.get(4)?;
            Ok((path_str, version, hash, size, mtime_ns))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (path_str, version, hash_vec, size, mtime_ns) = row?;
            let entry_path = match MemoryPath::parse(&path_str) {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Segment-boundary recheck: the range scan over `[prefix, prefix + U+10FFFF)`
            // correctly bounds at the prefix boundary for any reasonable prefix, but we
            // re-verify to be safe against any edge case where a path happens to sort
            // inside the range without being segment-boundary-inside the prefix.
            if !entry_path.starts_with_prefix(prefix) {
                continue;
            }
            let mut content_hash = [0u8; 32];
            if hash_vec.len() == 32 {
                content_hash.copy_from_slice(&hash_vec);
            }
            out.push(MemoryEntry {
                path: entry_path,
                size: size.max(0) as u64,
                version: MemoryVersion(version as u64),
                mtime: ns_to_system_time(mtime_ns),
                content_hash,
            });
        }
        Ok(out)
    }

    fn current_version(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
    ) -> Result<Option<MemoryVersion>, MemoryError> {
        let conn = self.open_conn(tenant)?;
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT version, deleted FROM memory_content WHERE path = ?1",
                params![path.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((version, 0)) => Some(MemoryVersion(version as u64)),
            _ => None,
        })
    }

    fn delete(&self, tenant: &TenantId, path: &MemoryPath) -> Result<MemoryVersion, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");

        let mut conn = self.open_conn(tenant)?;
        // BEGIN IMMEDIATE acquires the write lock upfront. This is critical
        // under WAL mode: a deferred transaction that upgrades from reader
        // to writer mid-stream returns BUSY immediately (to avoid deadlock)
        // and busy_timeout does NOT retry. IMMEDIATE lets busy_timeout do
        // its job.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let current: Option<i64> = tx
            .query_row(
                "SELECT version FROM memory_content WHERE path = ?1",
                params![path.as_str()],
                |row| row.get(0),
            )
            .optional()?;

        let Some(current_version) = current else {
            return Err(MemoryError::NotFound(path.as_str().to_string()));
        };

        let new_version = current_version + 1;
        tx.execute(
            "UPDATE memory_content
             SET version = ?1, deleted = 1, mtime_ns = ?2
             WHERE path = ?3",
            params![new_version, now_ns(), path.as_str()],
        )?;
        tx.commit()?;

        let version = MemoryVersion(new_version as u64);
        let _ = self.sender.send(MemoryEvent::Delete {
            tenant: tenant.clone(),
            path: path.clone(),
            version,
            produced_at: SystemTime::now(),
        });
        Ok(version)
    }

    fn delete_prefix(&self, tenant: &TenantId, prefix: &MemoryPath) -> Result<u64, MemoryError> {
        // Atomic batch: hold the per-tenant write lock once, run a single
        // transaction that bumps versions + tombstones every matching path
        // in one UPDATE. This prevents a concurrent put from landing in the
        // middle of a prefix delete and surviving. Event emission is
        // per-path but happens after the commit so subscribers only see
        // deleted rows that are actually deleted.
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");

        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let prefix_str = prefix.as_str();
        let upper = prefix_upper_bound(prefix_str);
        let mtime = now_ns();

        // Collect the list of affected (path, new_version) pairs BEFORE
        // updating, so we can emit events for them after commit.
        let mut affected: Vec<(MemoryPath, MemoryVersion)> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT path, version FROM memory_content
                 WHERE path >= ?1 AND path < ?2 AND deleted = 0
                 ORDER BY path ASC",
            )?;
            let rows = stmt.query_map(params![prefix_str, &upper], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (path_str, version) = row?;
                let Ok(path) = MemoryPath::parse(&path_str) else {
                    continue;
                };
                if !path.starts_with_prefix(prefix) {
                    continue;
                }
                let new_version = MemoryVersion((version + 1) as u64);
                affected.push((path, new_version));
            }
        }

        // Single UPDATE bumps version and sets deleted=1 for every path
        // in the range. Any new `put` that landed before this tx would
        // have taken the write lock, so it can't race with us here.
        tx.execute(
            "UPDATE memory_content
             SET version = version + 1, deleted = 1, mtime_ns = ?1
             WHERE path >= ?2 AND path < ?3 AND deleted = 0",
            params![mtime, prefix_str, upper],
        )?;
        tx.commit()?;

        let count = affected.len() as u64;
        for (path, version) in affected {
            let _ = self.sender.send(MemoryEvent::Delete {
                tenant: tenant.clone(),
                path,
                version,
                produced_at: SystemTime::now(),
            });
        }
        Ok(count)
    }

    fn subscribe(&self) -> Result<Box<dyn MemoryEventReceiver>, MemoryError> {
        Ok(Box::new(BroadcastEventReceiver {
            rx: self.sender.subscribe(),
        }))
    }

    /// S038: Eagerly open (and create if needed) the per-tenant SQLite DB
    /// and run the schema migration. Converts deferred runtime errors
    /// (corrupt file, bad schema) into startup errors. Idempotent.
    fn ensure_tenant(&self, tenant: &TenantId) -> Result<(), MemoryError> {
        let _conn = self.open_conn(tenant)?;
        Ok(())
    }
}

/// Concrete `MemoryEventReceiver` wrapping a tokio broadcast receiver.
///
/// Async consumers (the background embedder) should use `recv`, which
/// handles `Lagged` errors explicitly by returning `MemoryRecvOutcome::Lagged`
/// with the skipped count. Sync consumers can use `recv_blocking`, which
/// transparently skips lag events (the consumer cannot recover anyway).
///
/// `recv_blocking` MUST NOT be called from inside a tokio runtime — it would
/// deadlock. Use `recv` from async contexts.
pub struct BroadcastEventReceiver {
    rx: broadcast::Receiver<MemoryEvent>,
}

impl MemoryEventReceiver for BroadcastEventReceiver {
    fn recv<'a>(
        &'a mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = MemoryRecvOutcome> + Send + 'a>> {
        Box::pin(async move {
            match self.rx.recv().await {
                Ok(event) => MemoryRecvOutcome::Event(event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "memory event subscriber lagged");
                    MemoryRecvOutcome::Lagged { skipped }
                }
                Err(broadcast::error::RecvError::Closed) => MemoryRecvOutcome::Closed,
            }
        })
    }

    fn recv_blocking(&mut self) -> Option<MemoryEvent> {
        loop {
            match self.rx.blocking_recv() {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "memory event subscriber lagged (blocking)");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}
