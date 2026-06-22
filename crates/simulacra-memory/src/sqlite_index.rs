//! `SqliteVectorIndex` — concrete `VectorIndex` backed by per-tenant SQLite
//! databases with a `sqlite-vec` virtual table.
//!
//! Per S037 §6 each tenant owns one SQLite file at `{root}/memory/{tenant}.db`.
//! This implementation owns the following tables on that file:
//!
//! - `memory_schema_meta` — single-row (id = 1) record of `(embedder_id, dim,
//!   created_at)`. Written on first upsert to a tenant, then frozen.
//! - `memory_chunks` — chunk text + locator + version metadata.
//! - `memory_vectors` — `sqlite-vec` `vec0` virtual table; the `FLOAT[N]`
//!   dimension is templated from the configured embedder.
//! - `memory_embed_backlog` — deferred-embedding queue (written by the
//!   background embedder; we only create the table so it coexists on the file).
//! - `memory_embedder_log` — audit log of bulk reindex / wipe operations.
//! - `memory_path_tombstones` — per-path tombstone versions, used to reject
//!   stale upserts that race with deletes.
//!
//! The parallel [`crate::sqlite_store::SqliteMemoryStore`] owns `memory_content`
//! on the same DB file. Both implementations use `CREATE TABLE IF NOT EXISTS`
//! so they coexist without ordering requirements.
//!
//! **sqlite-vec loading:** `sqlite_vec::sqlite3_vec_init` is registered as an
//! `sqlite3_auto_extension` exactly once per process via [`std::sync::Once`],
//! so every `rusqlite::Connection` opened after the first index instance is
//! constructed — including the ones opened by `SqliteMemoryStore` — transparently
//! loads the extension.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use simulacra_types::{Locator, MEMORY_SNIPPET_CHARS, MemoryPath, MemoryVersion, TenantId};

use crate::embedder::EmbedderId;
use crate::error::MemoryError;
use crate::index::{
    BACKLOG_MAX_RETRIES, BacklogRow, IndexedChunk, SearchHit, UpsertOutcome, VectorIndex,
};

/// Busy timeout for every connection (spec floor 5s, we use 10s for headroom
/// so the store's concurrency tests that hold external locks for 5s succeed).
const BUSY_TIMEOUT_MS: u64 = 10_000;

/// L2-norm tolerance for the unit-vector invariant. `1e-5` matches the spec
/// contract in S037 §3 and the `Embedder` trait docs.
const UNIT_NORM_TOLERANCE: f32 = 1e-5;

/// Single source of truth for the `memory_chunks` DDL. Referenced by
/// `ensure_schema` (first startup) and `wipe_and_reopen` (post-drop
/// recreation) so the two code paths can never drift.
const MEMORY_CHUNKS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS memory_chunks (
    chunk_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    path            TEXT NOT NULL,
    version         INTEGER NOT NULL,
    chunk_index     INTEGER NOT NULL,
    locator_kind    TEXT NOT NULL,
    locator_payload TEXT NOT NULL,
    text            TEXT NOT NULL,
    embedder_id     TEXT NOT NULL,
    UNIQUE(path, version, chunk_index)
);

CREATE INDEX IF NOT EXISTS memory_chunks_by_path
    ON memory_chunks(path);
"#;

/// Apply the standard pragmas to a freshly-opened tenant connection.
/// Shared between `open_conn` (normal ctor path) and `wipe_and_reopen`
/// (pre-ctor migration path) so the two never drift.
fn apply_tenant_pragmas(conn: &Connection) -> Result<(), MemoryError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

// ─── sqlite-vec one-shot extension registration ──────────────────────────────

static VEC_INIT: Once = Once::new();

/// Register `sqlite3_vec_init` as an auto-extension so every `Connection`
/// opened afterwards (whether by this module or `SqliteMemoryStore`) loads
/// the `vec0` virtual-table module. Idempotent.
fn ensure_vec_extension_loaded() {
    VEC_INIT.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is the well-known SQLite extension entry
        // point exported by the sqlite-vec C library. Registering it as an
        // auto-extension is the documented loading mechanism. The transmute
        // converts the exported `extern "C" fn()` pointer to the entry-point
        // signature SQLite expects. This runs exactly once per process.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

// ─── SqliteVectorIndex ───────────────────────────────────────────────────────

type TenantLock = Arc<Mutex<()>>;

pub struct SqliteVectorIndex {
    root: PathBuf,
    embedder_id: EmbedderId,
    dim: usize,
    locks: Mutex<HashMap<TenantId, TenantLock>>,
}

impl SqliteVectorIndex {
    /// Open (or create) a vector index rooted at `root` for the given
    /// configured embedder. The `{root}/memory` directory is created if it
    /// does not exist. Per-tenant `.db` files are created lazily on first
    /// upsert to that tenant.
    ///
    /// The `embedder_id` fixes the dimension for every tenant DB created
    /// through this instance. A mismatching `embedder_id` passed to
    /// [`upsert`](Self::upsert) or [`search`](Self::search) is rejected with
    /// `MemoryError::EmbedderMismatch`.
    pub fn new(root: &Path, embedder_id: EmbedderId) -> Result<Self, MemoryError> {
        ensure_vec_extension_loaded();

        let memory_dir = root.join("memory");
        std::fs::create_dir_all(&memory_dir)?;

        let dim = embedder_id.dim().ok_or_else(|| {
            MemoryError::Internal(format!(
                "embedder id has no parseable dim: {}",
                embedder_id.as_str()
            ))
        })?;

        Ok(Self {
            root: root.to_path_buf(),
            embedder_id,
            dim,
            locks: Mutex::new(HashMap::new()),
        })
    }

    /// S037 §13 startup dispatch: read the stored `(embedder_id, dim)`
    /// for a tenant without going through the fingerprint-validating
    /// constructor. Returns `None` on a fresh tenant (no DB file, or
    /// no `memory_schema_meta` row). Returns `Some((id, dim))` if a
    /// prior session seeded the meta.
    pub fn read_fingerprint_at(
        root: &Path,
        tenant: &TenantId,
    ) -> Result<Option<(EmbedderId, usize)>, MemoryError> {
        ensure_vec_extension_loaded();
        let path = root
            .join("memory")
            .join(format!("{}.db", tenant.as_fs_segment()));
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open(&path)?;
        apply_tenant_pragmas(&conn)?;
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT embedder_id, dim FROM memory_schema_meta WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((id_str, dim)) => Ok(Some((EmbedderId(id_str), dim as usize))),
        }
    }

    /// S037 §13 startup dispatch: append a row to `memory_embedder_log`
    /// without requiring a reconciled index. Used by
    /// `reindex_background` after `mark_tenant_stale` to leave an
    /// audit trail. `wipe_and_rebuild` writes its own log row inside
    /// `wipe_and_reopen`, so this is for the non-wipe path only.
    pub fn append_embedder_log_at(
        root: &Path,
        tenant: &TenantId,
        embedder_id: &EmbedderId,
        chunk_count: u64,
        action: &str,
    ) -> Result<(), MemoryError> {
        ensure_vec_extension_loaded();
        let memory_dir = root.join("memory");
        std::fs::create_dir_all(&memory_dir)?;
        let path = memory_dir.join(format!("{}.db", tenant.as_fs_segment()));
        let conn = Connection::open(&path)?;
        apply_tenant_pragmas(&conn)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS memory_embedder_log (
                 applied_at  INTEGER NOT NULL,
                 embedder_id TEXT NOT NULL,
                 chunk_count INTEGER NOT NULL,
                 action      TEXT NOT NULL
             )",
            [],
        )?;
        conn.execute(
            "INSERT INTO memory_embedder_log (applied_at, embedder_id, chunk_count, action)
             VALUES (?1, ?2, ?3, ?4)",
            params![now_ns(), embedder_id.as_str(), chunk_count as i64, action],
        )?;
        Ok(())
    }

    fn db_path(&self, tenant: &TenantId) -> PathBuf {
        self.root
            .join("memory")
            .join(format!("{}.db", tenant.as_fs_segment()))
    }

    /// S037 §13 `wipe_and_rebuild`: drop and recreate the vector index
    /// schema for a tenant DB with a new embedder, and atomically stage
    /// a backlog of paths to re-embed. The new dim is derived from the
    /// embedder id.
    ///
    /// Runs BEFORE constructing a reconciled [`SqliteVectorIndex`] — the
    /// constructor would otherwise refuse a mismatched stored dim. All
    /// of the following happen in a single IMMEDIATE transaction so the
    /// on-disk state is never partially migrated — either the wipe
    /// commits in full (leaving the tenant staged for rebuild) or the DB
    /// retains its pre-wipe schema:
    ///
    /// 1. Drop `memory_vectors` and `memory_chunks`.
    /// 2. Recreate both at `new_dim` (empty).
    /// 3. Update `memory_schema_meta` to `(new_embedder_id, new_dim)`.
    /// 4. Clear `memory_embed_backlog` of any stale rows from a prior
    ///    lifecycle.
    /// 5. Seed `memory_embed_backlog` with one row per non-tombstoned
    ///    `memory_content` path, so the background worker will re-chunk
    ///    and re-embed from the source of truth on startup.
    ///
    /// Without step 4+5 in the same tx, a crash between wipe and backlog
    /// seed would leave the tenant with matching fingerprint, empty
    /// `memory_chunks`, and no rebuild work queued — "silently healthy"
    /// but actually empty.
    ///
    /// Returns the count of chunk rows that existed before the drop —
    /// for audit logging of the wipe.
    ///
    /// `memory_path_tombstones` and `memory_embedder_log` are preserved.
    ///
    /// Concurrency: assumes single-process-per-tenant operation (the
    /// project-wide invariant). Multi-process access to the same tenant
    /// DB is not supported and can produce mixed-embedder vectors if
    /// another process has the DB open.
    pub fn wipe_and_reopen(
        root: &Path,
        tenant: &TenantId,
        new_embedder_id: EmbedderId,
    ) -> Result<u64, MemoryError> {
        ensure_vec_extension_loaded();

        let new_dim = new_embedder_id.dim().ok_or_else(|| {
            MemoryError::Internal(format!(
                "embedder id has no parseable dim: {}",
                new_embedder_id.as_str()
            ))
        })?;

        let memory_dir = root.join("memory");
        std::fs::create_dir_all(&memory_dir)?;

        let path = memory_dir.join(format!("{}.db", tenant.as_fs_segment()));
        let mut conn = Connection::open(&path)?;
        apply_tenant_pragmas(&conn)?;

        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Count chunks before dropping. If the schema has never been
        // initialized for this tenant, the table is absent and we return 0.
        let chunks_table_exists: bool = tx
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_chunks'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        let cleared: i64 = if chunks_table_exists {
            tx.query_row("SELECT COUNT(*) FROM memory_chunks", [], |row| row.get(0))?
        } else {
            0
        };

        tx.execute("DROP TABLE IF EXISTS memory_vectors", [])?;
        tx.execute("DROP TABLE IF EXISTS memory_chunks", [])?;

        // Recreate memory_chunks with the shared DDL (see MEMORY_CHUNKS_DDL)
        // so this function and ensure_schema can never drift.
        tx.execute_batch(MEMORY_CHUNKS_DDL)?;

        // Recreate memory_vectors at the NEW dim. The templated DDL mirrors
        // ensure_schema's vec0 CREATE but is formatted with `new_dim` here so
        // the whole wipe is committed atomically. No window exists in which
        // a concurrent reader would observe a missing vec0 table.
        let vec_ddl = format!(
            "CREATE VIRTUAL TABLE memory_vectors USING vec0(\n\
             \x20   chunk_id INTEGER PRIMARY KEY,\n\
             \x20   embedding FLOAT[{dim}]\n\
             );",
            dim = new_dim
        );
        tx.execute(&vec_ddl, [])?;

        // Ensure memory_schema_meta exists, then upsert the new (id, dim).
        tx.execute(
            "CREATE TABLE IF NOT EXISTS memory_schema_meta (
                 id          INTEGER PRIMARY KEY CHECK (id = 1),
                 embedder_id TEXT NOT NULL,
                 dim         INTEGER NOT NULL,
                 created_at  INTEGER NOT NULL
             )",
            [],
        )?;
        tx.execute(
            "INSERT INTO memory_schema_meta (id, embedder_id, dim, created_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE
                SET embedder_id = excluded.embedder_id,
                    dim = excluded.dim",
            params![new_embedder_id.as_str(), new_dim as i64, now_ns()],
        )?;

        // Ensure the backlog + tombstones + embedder_log tables exist
        // (they normally do; defensive for first-time wipes on a tenant
        // whose index was never opened). Keep in sync with ensure_schema.
        tx.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memory_path_tombstones (
                path              TEXT PRIMARY KEY,
                tombstone_version INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS memory_embed_backlog (
                path        TEXT PRIMARY KEY,
                version     INTEGER NOT NULL,
                enqueued_at INTEGER NOT NULL,
                retry_count INTEGER NOT NULL DEFAULT 0,
                last_error  TEXT
            );

            CREATE INDEX IF NOT EXISTS memory_embed_backlog_enqueued
                ON memory_embed_backlog(enqueued_at);

            CREATE TABLE IF NOT EXISTS memory_embedder_log (
                applied_at  INTEGER NOT NULL,
                embedder_id TEXT NOT NULL,
                chunk_count INTEGER NOT NULL,
                action      TEXT NOT NULL
            );
            "#,
        )?;

        // Stage the rebuild: clear any stale backlog rows from a prior
        // lifecycle, then seed one row per non-tombstoned memory_content
        // path. The background embedder will re-chunk + re-embed each one
        // from the content blob after the new index is spawned.
        //
        // If memory_content is absent (store never opened), the INSERT
        // matches zero rows — the wipe commits and the tenant is ready
        // for fresh content.
        tx.execute("DELETE FROM memory_embed_backlog", [])?;
        let content_exists: bool = tx
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_content'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if content_exists {
            tx.execute(
                "INSERT INTO memory_embed_backlog (path, version, enqueued_at)
                 SELECT path, version, ?1 FROM memory_content WHERE deleted = 0",
                params![now_ns()],
            )?;
        }

        // Audit log: record the wipe in the same tx so the history and
        // the on-disk state commit atomically. Caller does not need a
        // separate log-write step.
        tx.execute(
            "INSERT INTO memory_embedder_log (applied_at, embedder_id, chunk_count, action)
             VALUES (?1, ?2, ?3, 'wipe_and_rebuild')",
            params![now_ns(), new_embedder_id.as_str(), cleared],
        )?;

        tx.commit()?;

        Ok(cleared as u64)
    }

    /// S037 §13 same-dim reindex: update `memory_schema_meta.embedder_id`
    /// for a tenant DB without going through the fingerprint-validating
    /// constructor.
    ///
    /// Intended to run AFTER `mark_tenant_stale` +
    /// `enqueue_backlog_from_chunks` have staged the re-embed work, and
    /// BEFORE any worker is spawned against the tenant — the worker's
    /// constructor fingerprint check would otherwise reject it. Task 9's
    /// startup dispatch is responsible for that ordering.
    ///
    /// Rejects if `new_embedder_id.dim()` does not match the stored
    /// `memory_schema_meta.dim` (dim changes belong to `wipe_and_reopen`,
    /// not this helper).
    ///
    /// Idempotent. On a fresh tenant (no DB file yet), creates the
    /// `memory_schema_meta` table and seeds the row with the new
    /// embedder; the subsequent constructor call proceeds normally.
    ///
    /// Concurrency: assumes single-process-per-tenant operation. A
    /// concurrent writer in another process that passed its
    /// fingerprint check against the old embedder BEFORE this commit
    /// could subsequently write old-embedder vectors under the new
    /// fingerprint. Multi-process access to the same tenant DB is not
    /// supported by the design.
    pub fn set_embedder_id_at(
        root: &Path,
        tenant: &TenantId,
        new_embedder_id: &EmbedderId,
    ) -> Result<(), MemoryError> {
        ensure_vec_extension_loaded();

        let memory_dir = root.join("memory");
        std::fs::create_dir_all(&memory_dir)?;

        let path = memory_dir.join(format!("{}.db", tenant.as_fs_segment()));
        let mut conn = Connection::open(&path)?;
        apply_tenant_pragmas(&conn)?;

        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "CREATE TABLE IF NOT EXISTS memory_schema_meta (
                 id          INTEGER PRIMARY KEY CHECK (id = 1),
                 embedder_id TEXT NOT NULL,
                 dim         INTEGER NOT NULL,
                 created_at  INTEGER NOT NULL
             )",
            [],
        )?;
        let new_dim = new_embedder_id.dim().ok_or_else(|| {
            MemoryError::Internal(format!(
                "embedder id has no parseable dim: {}",
                new_embedder_id.as_str()
            ))
        })?;

        // Reject dim mismatches up-front so we never leave the stored
        // dim pointing at a vec0 table that doesn't match. Dim changes
        // belong to wipe_and_reopen.
        let stored_dim: Option<i64> = tx
            .query_row(
                "SELECT dim FROM memory_schema_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(stored_dim) = stored_dim {
            let stored_dim = stored_dim as usize;
            if stored_dim != new_dim {
                return Err(MemoryError::EmbedderDimensionMismatch {
                    stored: stored_dim,
                    configured: new_dim,
                });
            }
        }

        tx.execute(
            "INSERT INTO memory_schema_meta (id, embedder_id, dim, created_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET embedder_id = excluded.embedder_id",
            params![new_embedder_id.as_str(), new_dim as i64, now_ns()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn tenant_lock(&self, tenant: &TenantId) -> TenantLock {
        let mut locks = self.locks.lock().expect("tenant lock map poisoned");
        locks
            .entry(tenant.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Open a fresh connection and apply the standard pragmas. Creates the
    /// schema (including the templated `memory_vectors` DDL) if missing.
    fn open_conn(&self, tenant: &TenantId) -> Result<Connection, MemoryError> {
        let path = self.db_path(tenant);
        let conn = Connection::open(&path)?;
        apply_tenant_pragmas(&conn)?;
        self.ensure_schema(&conn)?;
        Ok(conn)
    }

    fn ensure_schema(&self, conn: &Connection) -> Result<(), MemoryError> {
        // Fixed tables. All use IF NOT EXISTS so this coexists with
        // SqliteMemoryStore's ensure_schema on the same DB file.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memory_schema_meta (
                id          INTEGER PRIMARY KEY CHECK (id = 1),
                embedder_id TEXT NOT NULL,
                dim         INTEGER NOT NULL,
                created_at  INTEGER NOT NULL
            );
            "#,
        )?;
        conn.execute_batch(MEMORY_CHUNKS_DDL)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memory_path_tombstones (
                path              TEXT PRIMARY KEY,
                tombstone_version INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS memory_embed_backlog (
                path        TEXT PRIMARY KEY,
                version     INTEGER NOT NULL,
                enqueued_at INTEGER NOT NULL,
                retry_count INTEGER NOT NULL DEFAULT 0,
                last_error  TEXT
            );

            CREATE INDEX IF NOT EXISTS memory_embed_backlog_enqueued
                ON memory_embed_backlog(enqueued_at);

            CREATE TABLE IF NOT EXISTS memory_embedder_log (
                applied_at  INTEGER NOT NULL,
                embedder_id TEXT NOT NULL,
                chunk_count INTEGER NOT NULL,
                action      TEXT NOT NULL
            );
            "#,
        )?;

        // Check stored embedder fingerprint BEFORE creating the templated
        // virtual table. If an existing meta row names a different dim, we
        // must NOT run the CREATE VIRTUAL TABLE with a new dim (the
        // existing table's schema is fixed) and we must NOT silently let
        // mixed-dim rows accumulate.
        //
        // Per S037 §13, model-change enforcement is:
        //   - Same embedder (name, version, dim): normal operation
        //   - Same dim, different name/version: refuse (requires explicit
        //     reindex_background policy, which wipes vectors — that's a
        //     separate codepath that re-opens the index after calling
        //     mark_tenant_stale).
        //   - Different dim: refuse with requires_wipe=true (only
        //     wipe_and_rebuild can change the dim).
        //
        // For MVP, any mismatch returns an error from `new`. Callers that
        // want to reindex or wipe run that flow BEFORE constructing a new
        // SqliteVectorIndex with the new embedder.
        let existing: Option<(String, i64)> = conn
            .query_row(
                "SELECT embedder_id, dim FROM memory_schema_meta WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_id, stored_dim)) = existing {
            let stored_dim = stored_dim as usize;
            if stored_dim != self.dim {
                return Err(MemoryError::EmbedderDimensionMismatch {
                    stored: stored_dim,
                    configured: self.dim,
                });
            }
            if stored_id != self.embedder_id.as_str() {
                return Err(MemoryError::EmbedderMismatch {
                    stored: stored_id,
                    configured: self.embedder_id.as_str().to_string(),
                    requires_wipe: false, // same dim — reindex_background is viable
                });
            }
        }

        // Templated virtual table. The dim is interpolated from the configured
        // embedder; once the table exists it is frozen for the lifetime of
        // the tenant DB. Same-dim model swap does NOT recreate the table.
        let vec_ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memory_vectors USING vec0(\n\
             \x20   chunk_id INTEGER PRIMARY KEY,\n\
             \x20   embedding FLOAT[{dim}]\n\
             );",
            dim = self.dim
        );
        conn.execute(&vec_ddl, [])?;

        // Seed the schema meta row on first creation. The single-row CHECK
        // constraint + ON CONFLICT DO NOTHING keeps this idempotent.
        conn.execute(
            "INSERT INTO memory_schema_meta (id, embedder_id, dim, created_at)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO NOTHING",
            params![self.embedder_id.as_str(), self.dim as i64, now_ns()],
        )?;

        Ok(())
    }

    /// Verify that `embedder_id` matches the dim recorded in the schema meta
    /// row. Exact-match on `embedder_id` is enforced by the caller (the
    /// trait contract in §13) via the checks in `upsert` / `search`.
    fn check_dim(&self, incoming: &EmbedderId) -> Result<(), MemoryError> {
        let got = incoming.dim().unwrap_or(0);
        if got != self.dim {
            return Err(MemoryError::EmbedderDimensionMismatch {
                stored: self.dim,
                configured: got,
            });
        }
        Ok(())
    }
}

// ─── Shared helpers ──────────────────────────────────────────────────────────

/// Compute the exclusive upper bound for a prefix range scan. See
/// `sqlite_store::prefix_upper_bound` for the rationale — duplicated here to
/// keep the two implementations independent.
fn prefix_upper_bound(prefix: &str) -> String {
    let mut upper = String::with_capacity(prefix.len() + 4);
    upper.push_str(prefix);
    upper.push('\u{10FFFF}');
    upper
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before UNIX_EPOCH")
        .as_nanos() as i64
}

/// Serialize a `Vec<f32>` to the little-endian byte buffer that `sqlite-vec`
/// expects when bound as a blob for a `FLOAT[N]` column.
/// Inverse of `embedding_to_bytes` — decode a `FLOAT[N]` blob back into
/// a `Vec<f32>`. Returns empty if the blob length is not a multiple of 4.
fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    if !bytes.len().is_multiple_of(4) {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn embedding_to_bytes(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// Compute the L2 norm of a vector.
fn l2_norm(values: &[f32]) -> f32 {
    values.iter().map(|v| v * v).sum::<f32>().sqrt()
}

fn is_unit_norm(values: &[f32]) -> Result<(), MemoryError> {
    let norm = l2_norm(values);
    if (norm - 1.0).abs() > UNIT_NORM_TOLERANCE {
        return Err(MemoryError::NotUnitVector(norm));
    }
    Ok(())
}

/// Map a `Locator` variant to the `locator_kind` discriminator stored in
/// `memory_chunks`. Matches the `#[serde(tag = "kind", rename_all = "snake_case")]`
/// attribute on the enum so the JSON payload and the column agree.
fn locator_kind(locator: &Locator) -> &'static str {
    match locator {
        Locator::Text { .. } => "text",
        Locator::PdfPage { .. } => "pdf_page",
        Locator::HtmlSelector { .. } => "html_selector",
        Locator::JsonlLine { .. } => "jsonl_line",
        Locator::Opaque { .. } => "opaque",
    }
}

fn encode_locator(locator: &Locator) -> Result<String, MemoryError> {
    serde_json::to_string(locator)
        .map_err(|e| MemoryError::Internal(format!("failed to encode locator: {e}")))
}

fn decode_locator(payload: &str) -> Result<Locator, MemoryError> {
    serde_json::from_str(payload)
        .map_err(|e| MemoryError::Internal(format!("failed to decode locator: {e}")))
}

/// Take the first `MEMORY_SNIPPET_CHARS` (characters, not bytes) from the
/// chunk text. Uses `chars().take(n).collect::<String>()` so we never split a
/// multi-byte UTF-8 sequence.
fn snippet_for(text: &str) -> String {
    text.chars().take(MEMORY_SNIPPET_CHARS).collect()
}

// ─── VectorIndex impl ────────────────────────────────────────────────────────

impl VectorIndex for SqliteVectorIndex {
    fn upsert(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
        embedder_id: &EmbedderId,
        chunks: &[IndexedChunk],
    ) -> Result<UpsertOutcome, MemoryError> {
        // Fast rejections before taking the write lock:
        //   * embedder identity (exact id match; dim is a subset of this but
        //     we keep the more-specific error for callers)
        //   * unit-vector invariant on every chunk
        if embedder_id != &self.embedder_id {
            return Err(MemoryError::EmbedderMismatch {
                stored: self.embedder_id.0.clone(),
                configured: embedder_id.0.clone(),
                requires_wipe: embedder_id.dim() != Some(self.dim),
            });
        }
        self.check_dim(embedder_id)?;
        for chunk in chunks {
            if chunk.embedding.len() != self.dim {
                return Err(MemoryError::VectorDimMismatch {
                    expected: self.dim,
                    got: chunk.embedding.len(),
                });
            }
            is_unit_norm(&chunk.embedding)?;
        }

        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");

        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let submitted = version.0 as i64;

        // Tombstone guard: if a tombstone exists at >= submitted, reject.
        let tombstone: Option<i64> = tx
            .query_row(
                "SELECT tombstone_version FROM memory_path_tombstones WHERE path = ?1",
                params![path.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(tombstone_version) = tombstone
            && tombstone_version >= submitted
        {
            return Ok(UpsertOutcome::Tombstoned);
        }

        // Stale guard: if stored version is >= submitted, drop the upsert.
        let stored: Option<i64> = tx
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM memory_chunks WHERE path = ?1",
                params![path.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(stored_version) = stored
            && stored_version >= submitted
            && stored_version > 0
        {
            return Ok(UpsertOutcome::Stale);
        }

        // Applied path: clear existing chunks + vectors for this path, then
        // insert the new set in a single atomic transaction.
        {
            // Collect chunk_ids first so we can delete their vec0 rows.
            let mut select = tx.prepare("SELECT chunk_id FROM memory_chunks WHERE path = ?1")?;
            let old_ids: Vec<i64> = select
                .query_map(params![path.as_str()], |row| row.get::<_, i64>(0))?
                .collect::<Result<_, _>>()?;
            drop(select);

            for old_id in &old_ids {
                tx.execute(
                    "DELETE FROM memory_vectors WHERE chunk_id = ?1",
                    params![old_id],
                )?;
            }
            tx.execute(
                "DELETE FROM memory_chunks WHERE path = ?1",
                params![path.as_str()],
            )?;
        }

        for chunk in chunks {
            let kind = locator_kind(&chunk.locator);
            let payload = encode_locator(&chunk.locator)?;
            tx.execute(
                "INSERT INTO memory_chunks
                    (path, version, chunk_index, locator_kind, locator_payload, text, embedder_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    path.as_str(),
                    submitted,
                    chunk.chunk_index as i64,
                    kind,
                    payload,
                    chunk.text,
                    embedder_id.as_str(),
                ],
            )?;
            let chunk_id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO memory_vectors (chunk_id, embedding) VALUES (?1, ?2)",
                params![chunk_id, embedding_to_bytes(&chunk.embedding)],
            )?;
        }

        tx.execute(
            "INSERT INTO memory_embedder_log (applied_at, embedder_id, chunk_count, action)
             VALUES (?1, ?2, ?3, 'upsert')",
            params![now_ns(), embedder_id.as_str(), chunks.len() as i64,],
        )?;

        tx.commit()?;
        Ok(UpsertOutcome::Applied)
    }

    fn delete_path(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        tombstone_version: MemoryVersion,
    ) -> Result<(), MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");

        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let version_i = tombstone_version.0 as i64;

        // Record or bump the tombstone. Take the MAX so late-arriving lower
        // tombstones cannot regress a higher recorded tombstone.
        tx.execute(
            "INSERT INTO memory_path_tombstones (path, tombstone_version)
             VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET
                tombstone_version = MAX(tombstone_version, excluded.tombstone_version)",
            params![path.as_str(), version_i],
        )?;

        // Collect and delete the chunks (so the vec0 rows also go).
        let old_ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT chunk_id FROM memory_chunks WHERE path = ?1")?;
            stmt.query_map(params![path.as_str()], |row| row.get::<_, i64>(0))?
                .collect::<Result<_, _>>()?
        };
        for id in &old_ids {
            tx.execute(
                "DELETE FROM memory_vectors WHERE chunk_id = ?1",
                params![id],
            )?;
        }
        tx.execute(
            "DELETE FROM memory_chunks WHERE path = ?1",
            params![path.as_str()],
        )?;

        let removed = old_ids.len() as i64;
        tx.execute(
            "INSERT INTO memory_embedder_log (applied_at, embedder_id, chunk_count, action)
             VALUES (?1, ?2, ?3, 'delete')",
            params![now_ns(), self.embedder_id.as_str(), removed],
        )?;

        tx.commit()?;
        Ok(())
    }

    fn delete_prefix(&self, tenant: &TenantId, prefix: &MemoryPath) -> Result<u64, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");

        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let prefix_str = prefix.as_str();
        let upper = prefix_upper_bound(prefix_str);

        // Collect affected paths + versions + chunk_ids. We need:
        //   - chunk_ids to delete from memory_vectors
        //   - per-path max(chunk version, tombstone version) so the new
        //     tombstone is strictly greater than both, guaranteeing that any
        //     late-arriving queued upsert for this path cannot resurrect it.
        struct Row {
            path: String,
            chunk_id: i64,
            version: i64,
        }
        let rows: Vec<Row> = {
            let mut stmt = tx.prepare(
                "SELECT path, chunk_id, version FROM memory_chunks
                 WHERE path >= ?1 AND path < ?2",
            )?;
            stmt.query_map(params![prefix_str, upper.as_str()], |row| {
                Ok(Row {
                    path: row.get(0)?,
                    chunk_id: row.get(1)?,
                    version: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?
        };

        // Per-path: {max chunk version, total chunks removed}.
        let mut per_path: std::collections::BTreeMap<String, (i64, u64)> =
            std::collections::BTreeMap::new();
        let mut chunk_rows_removed: u64 = 0;
        for row in &rows {
            // Segment-boundary filter: belt and braces over the range scan.
            let Ok(row_path) = MemoryPath::parse(&row.path) else {
                continue;
            };
            if !row_path.starts_with_prefix(prefix) {
                continue;
            }
            tx.execute(
                "DELETE FROM memory_vectors WHERE chunk_id = ?1",
                params![row.chunk_id],
            )?;
            let entry = per_path.entry(row.path.clone()).or_insert((0, 0));
            entry.0 = entry.0.max(row.version);
            entry.1 += 1;
            chunk_rows_removed += 1;
        }

        // Delete memory_chunks rows for each affected path, and write a
        // tombstone version strictly greater than both the current chunk
        // version AND any existing tombstone version for that path. This is
        // the resurrection-safety guarantee — a late upsert at version N
        // where N <= the new tombstone will be dropped by `upsert`'s
        // `UpsertOutcome::Tombstoned` path.
        for (path, (max_chunk_version, _)) in &per_path {
            tx.execute("DELETE FROM memory_chunks WHERE path = ?1", params![path])?;

            // Existing tombstone (if any).
            let existing_tomb: Option<i64> = tx
                .query_row(
                    "SELECT tombstone_version FROM memory_path_tombstones WHERE path = ?1",
                    params![path],
                    |row| row.get(0),
                )
                .optional()?;
            let new_tomb = std::cmp::max(*max_chunk_version, existing_tomb.unwrap_or(0)) + 1;
            tx.execute(
                "INSERT INTO memory_path_tombstones (path, tombstone_version)
                 VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET
                     tombstone_version = MAX(tombstone_version, excluded.tombstone_version)",
                params![path, new_tomb],
            )?;
        }

        // The trait says delete_prefix returns the number of CHUNKS removed.
        tx.execute(
            "INSERT INTO memory_embedder_log (applied_at, embedder_id, chunk_count, action)
             VALUES (?1, ?2, ?3, 'delete')",
            params![
                now_ns(),
                self.embedder_id.as_str(),
                chunk_rows_removed as i64
            ],
        )?;

        tx.commit()?;
        Ok(chunk_rows_removed)
    }

    fn search(
        &self,
        tenant: &TenantId,
        scope: &MemoryPath,
        query_embedding: &[f32],
        embedder_id: &EmbedderId,
        k: usize,
        min_cosine: Option<f32>,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        if embedder_id != &self.embedder_id {
            return Err(MemoryError::EmbedderMismatch {
                stored: self.embedder_id.0.clone(),
                configured: embedder_id.0.clone(),
                requires_wipe: embedder_id.dim() != Some(self.dim),
            });
        }
        self.check_dim(embedder_id)?;
        if query_embedding.len() != self.dim {
            return Err(MemoryError::VectorDimMismatch {
                expected: self.dim,
                got: query_embedding.len(),
            });
        }
        is_unit_norm(query_embedding)?;

        let conn = self.open_conn(tenant)?;
        let scope_str = scope.as_str();
        let upper = prefix_upper_bound(scope_str);
        let query_blob = embedding_to_bytes(query_embedding);

        // Over-fetch slightly so that the segment-boundary filter and the
        // min_cosine filter still leave us with `k` rows where possible. A
        // factor of 4 plus a small floor is conservative without being
        // wasteful.
        let fetch = (k.saturating_mul(4)).max(k + 8);

        let mut stmt = conn.prepare(
            "SELECT c.path, c.chunk_index, c.version, c.locator_kind, c.locator_payload, c.text,
                    vec_distance_cosine(v.embedding, ?1) AS distance
             FROM memory_chunks c
             JOIN memory_vectors v ON v.chunk_id = c.chunk_id
             WHERE c.path >= ?2 AND c.path < ?3
             ORDER BY distance ASC
             LIMIT ?4",
        )?;

        let rows = stmt.query_map(
            params![query_blob, scope_str, upper.as_str(), fetch as i64],
            |row| {
                let path: String = row.get(0)?;
                let chunk_index: i64 = row.get(1)?;
                let version: i64 = row.get(2)?;
                let _kind: String = row.get(3)?;
                let payload: String = row.get(4)?;
                let text: String = row.get(5)?;
                let distance: f64 = row.get(6)?;
                Ok((path, chunk_index, version, payload, text, distance))
            },
        )?;

        let mut out: Vec<SearchHit> = Vec::new();
        for row in rows {
            let (path_str, chunk_index, version, payload, text, distance) = row?;
            let path = match MemoryPath::parse(&path_str) {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Segment-boundary filter: the range scan may catch near-prefix
            // matches like `/var/memory/selfish` when the scope is
            // `/var/memory/self`. Reject those at the Rust level.
            if !path.starts_with_prefix(scope) {
                continue;
            }
            let cosine = 1.0_f32 - distance as f32;
            if let Some(min) = min_cosine
                && cosine < min
            {
                continue;
            }
            let locator = decode_locator(&payload)?;
            out.push(SearchHit {
                path,
                chunk_index: chunk_index as usize,
                version: MemoryVersion(version as u64),
                locator,
                snippet: snippet_for(&text),
                cosine_score: cosine,
            });
            if out.len() >= k {
                break;
            }
        }

        Ok(out)
    }

    fn embedder_fingerprint(&self, tenant: &TenantId) -> Result<Option<EmbedderId>, MemoryError> {
        let db = self.db_path(tenant);
        if !db.exists() {
            return Ok(None);
        }
        let conn = self.open_conn(tenant)?;
        let stored: Option<String> = conn
            .query_row(
                "SELECT embedder_id FROM memory_schema_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored.map(EmbedderId))
    }

    fn get_chunk(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
        chunk_index: usize,
    ) -> Result<Option<(Locator, String)>, MemoryError> {
        let db = self.db_path(tenant);
        if !db.exists() {
            return Ok(None);
        }
        let conn = self.open_conn(tenant)?;
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT locator_payload, text FROM memory_chunks
                 WHERE path = ?1 AND version = ?2 AND chunk_index = ?3",
                params![path.as_str(), version.0 as i64, chunk_index as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match row {
            Some((payload, text)) => {
                let locator = decode_locator(&payload)?;
                Ok(Some((locator, text)))
            }
            None => Ok(None),
        }
    }

    fn mark_tenant_stale(&self, tenant: &TenantId) -> Result<u64, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");

        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let cleared: i64 =
            tx.query_row("SELECT COUNT(*) FROM memory_vectors", [], |row| row.get(0))?;

        tx.execute("DELETE FROM memory_vectors", [])?;

        tx.execute(
            "INSERT INTO memory_embedder_log (applied_at, embedder_id, chunk_count, action)
             VALUES (?1, ?2, ?3, 'reindex')",
            params![now_ns(), self.embedder_id.as_str(), cleared],
        )?;

        tx.commit()?;
        Ok(cleared as u64)
    }

    /// S037 §13: insert one `memory_embed_backlog` row per distinct path
    /// in `memory_chunks`, stamped with the version currently in
    /// `memory_chunks`. The backlog `PRIMARY KEY` is on `path` alone, so
    /// `INSERT OR IGNORE` makes repeat calls a no-op; intended to be run
    /// at startup BEFORE the background embedder is spawned, because
    /// concurrent writers to the backlog would see `INSERT OR IGNORE`
    /// silently skip newer versions.
    fn enqueue_backlog_from_chunks(&self, tenant: &TenantId) -> Result<u64, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "-- DISTINCT collapses N chunks per path; backlog PK is path only
             INSERT OR IGNORE INTO memory_embed_backlog (path, version, enqueued_at)
             SELECT DISTINCT path, version, ?1 FROM memory_chunks",
            params![now_ns()],
        )?;
        tx.commit()?;
        Ok(inserted as u64)
    }

    /// S037 §13: seed the embed backlog from `memory_content` after a
    /// `wipe_and_rebuild` has dropped all chunks. Skips tombstones
    /// (`deleted = 1`) so reaped paths do not re-embed. Idempotent via
    /// `INSERT OR IGNORE` on the backlog's `path` PK.
    ///
    /// Deliberately crosses the store/index owner boundary: `memory_content`
    /// is written by `SqliteMemoryStore` but co-located in the same per-tenant
    /// DB file, so this is a single-connection read. If the store and index
    /// ever live in separate DBs, this query must move into the store.
    fn enqueue_backlog_from_content(&self, tenant: &TenantId) -> Result<u64, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "-- Backlog PK is path; memory_content is already unique on path.
             INSERT OR IGNORE INTO memory_embed_backlog (path, version, enqueued_at)
             SELECT path, version, ?1 FROM memory_content WHERE deleted = 0",
            params![now_ns()],
        )?;
        tx.commit()?;
        Ok(inserted as u64)
    }

    /// S037 §8: record a `(path, version)` pair in `memory_embed_backlog`
    /// for the overflow path. Strict-version-wins semantics — see the
    /// trait doc for `enqueue_backlog_for`.
    ///
    /// Atomic via a single `INSERT ... ON CONFLICT(path) DO UPDATE`:
    /// each CASE branches on `excluded.version > version` so a same-or-
    /// older incoming version is a no-op, while a strictly newer version
    /// advances the row AND resets retry_count/last_error/enqueued_at.
    fn enqueue_backlog_for(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
    ) -> Result<(), MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");

        let conn = self.open_conn(tenant)?;
        conn.execute(
            "INSERT INTO memory_embed_backlog (path, version, enqueued_at, retry_count, last_error)
             VALUES (?1, ?2, ?3, 0, NULL)
             ON CONFLICT(path) DO UPDATE SET
                version     = CASE WHEN excluded.version > version THEN excluded.version ELSE version END,
                retry_count = CASE WHEN excluded.version > version THEN 0 ELSE retry_count END,
                last_error  = CASE WHEN excluded.version > version THEN NULL ELSE last_error END,
                enqueued_at = CASE WHEN excluded.version > version THEN excluded.enqueued_at ELSE enqueued_at END",
            params![path.as_str(), version.0 as i64, now_ns()],
        )?;
        Ok(())
    }

    fn load_chunks_for(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
    ) -> Result<Vec<IndexedChunk>, MemoryError> {
        let conn = self.open_conn(tenant)?;
        let mut stmt = conn.prepare(
            "SELECT c.chunk_index, c.locator_kind, c.locator_payload, c.text, v.embedding
               FROM memory_chunks c
               LEFT JOIN memory_vectors v ON v.chunk_id = c.chunk_id
              WHERE c.path = ?1 AND c.version = ?2
              ORDER BY c.chunk_index ASC",
        )?;
        let rows = stmt
            .query_map(params![path.as_str(), version.0 as i64], |row| {
                let chunk_index: i64 = row.get(0)?;
                let kind: String = row.get(1)?;
                let payload: String = row.get(2)?;
                let text: String = row.get(3)?;
                let embedding_bytes: Option<Vec<u8>> = row.get(4)?;
                Ok((chunk_index, kind, payload, text, embedding_bytes))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (chunk_index, _kind, payload, text, embedding_bytes) in rows {
            let locator = decode_locator(&payload)?;
            let embedding = match embedding_bytes {
                Some(bytes) => bytes_to_embedding(&bytes),
                None => Vec::new(),
            };
            out.push(IndexedChunk {
                chunk_index: chunk_index as usize,
                locator,
                text,
                embedding,
            });
        }
        Ok(out)
    }

    fn upsert_chunks_only(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
        chunks: &[IndexedChunk],
    ) -> Result<(), MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Replace any existing chunks at this path (regardless of version)
        // to mirror the `upsert` semantics — one live version per path.
        let existing_chunk_ids: Vec<i64> = tx
            .prepare("SELECT chunk_id FROM memory_chunks WHERE path = ?1")?
            .query_map(params![path.as_str()], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !existing_chunk_ids.is_empty() {
            for chunk_id in &existing_chunk_ids {
                tx.execute(
                    "DELETE FROM memory_vectors WHERE chunk_id = ?1",
                    params![chunk_id],
                )?;
            }
            tx.execute(
                "DELETE FROM memory_chunks WHERE path = ?1",
                params![path.as_str()],
            )?;
        }
        for chunk in chunks {
            let kind = locator_kind(&chunk.locator);
            let payload = encode_locator(&chunk.locator)?;
            tx.execute(
                "INSERT INTO memory_chunks
                    (path, version, chunk_index, locator_kind, locator_payload, text, embedder_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    path.as_str(),
                    version.0 as i64,
                    chunk.chunk_index as i64,
                    kind,
                    payload,
                    chunk.text,
                    self.embedder_id.as_str(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn write_vectors_for_chunks(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
        embedder_id: &EmbedderId,
        embeddings: &[Vec<f32>],
    ) -> Result<(), MemoryError> {
        // Reject dim mismatch up-front.
        for emb in embeddings {
            if emb.len() != self.dim {
                return Err(MemoryError::VectorDimMismatch {
                    expected: self.dim,
                    got: emb.len(),
                });
            }
            is_unit_norm(emb)?;
        }

        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let chunk_ids: Vec<i64> = tx
            .prepare(
                "SELECT chunk_id FROM memory_chunks
                  WHERE path = ?1 AND version = ?2
                  ORDER BY chunk_index ASC",
            )?
            .query_map(params![path.as_str(), version.0 as i64], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        if chunk_ids.len() != embeddings.len() {
            return Err(MemoryError::Internal(format!(
                "write_vectors_for_chunks: chunk count mismatch — {} chunks in DB, {} embeddings provided",
                chunk_ids.len(),
                embeddings.len(),
            )));
        }

        for (chunk_id, emb) in chunk_ids.iter().zip(embeddings.iter()) {
            // Replace any existing vector row at this chunk_id.
            tx.execute(
                "DELETE FROM memory_vectors WHERE chunk_id = ?1",
                params![chunk_id],
            )?;
            tx.execute(
                "INSERT INTO memory_vectors (chunk_id, embedding) VALUES (?1, ?2)",
                params![chunk_id, embedding_to_bytes(emb)],
            )?;
            // Update the chunk's embedder_id stamp so subsequent integrity
            // checks see the current embedder.
            tx.execute(
                "UPDATE memory_chunks SET embedder_id = ?2 WHERE chunk_id = ?1",
                params![chunk_id, embedder_id.as_str()],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    fn take_backlog_batch(
        &self,
        tenant: &TenantId,
        batch_size: usize,
    ) -> Result<Vec<BacklogRow>, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let conn = self.open_conn(tenant)?;
        // Dead-letter filter: rows whose retry_count has reached
        // BACKLOG_MAX_RETRIES remain in the table for operator inspection
        // but must be invisible to the drainer. Without this, the drainer
        // would repeatedly pull dead-lettered rows, skip them in-loop, and
        // treat the batch as "work done" — hot-spinning across passes.
        // Filtering here means the drainer sees an empty batch once every
        // row is either drainable or capped, and falls into its idle sleep.
        let mut stmt = conn.prepare(
            "SELECT path, version, retry_count
               FROM memory_embed_backlog
              WHERE retry_count < ?2
              ORDER BY retry_count ASC, enqueued_at ASC
              LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(
                params![batch_size as i64, BACKLOG_MAX_RETRIES as i64],
                |row| {
                    let path_str: String = row.get(0)?;
                    let version: i64 = row.get(1)?;
                    let retry_count: i64 = row.get(2)?;
                    Ok((path_str, version, retry_count))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (path_str, version, retry_count) in rows {
            let path = MemoryPath::parse(&path_str)
                .map_err(|e| MemoryError::Internal(format!("backlog path parse failed: {e}")))?;
            out.push(BacklogRow {
                path,
                version: MemoryVersion(version as u64),
                retry_count: retry_count.max(0) as u32,
            });
        }
        Ok(out)
    }

    fn delete_backlog_row(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
    ) -> Result<(), MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM memory_embed_backlog WHERE path = ?1 AND version = ?2",
            params![path.as_str(), version.0 as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn bump_backlog_retry(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
        last_error: &str,
    ) -> Result<(), MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE memory_embed_backlog
                SET retry_count = retry_count + 1,
                    last_error  = ?3
              WHERE path = ?1 AND version = ?2",
            params![path.as_str(), version.0 as i64, last_error],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn backlog_count(&self, tenant: &TenantId) -> Result<u64, MemoryError> {
        let conn = self.open_conn(tenant)?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM memory_embed_backlog", [], |row| {
                row.get(0)
            })?;
        Ok(count.max(0) as u64)
    }

    /// Enumerate every tenant that has a per-tenant SQLite database on
    /// disk under `{root}/memory/`. The tenant id is recovered by
    /// stripping the `.db` suffix from each filename and re-parsing
    /// through [`TenantId::parse`], which drops anything that isn't a
    /// valid tenant id (stray files, WAL/SHM sidecars, hidden files).
    ///
    /// Used by observability (`simulacra_memory_reindex_backlog`) to drive
    /// the per-tenant gauge even when the in-memory embedder has never
    /// seen events for a tenant.
    fn known_tenants(&self) -> Result<Vec<TenantId>, MemoryError> {
        let memory_dir = self.root.join("memory");
        let entries = match std::fs::read_dir(&memory_dir) {
            Ok(r) => r,
            // Fresh root where no tenant has been created yet — not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(MemoryError::from(e)),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".db") else {
                continue;
            };
            if let Ok(tenant) = TenantId::parse(stem) {
                out.push(tenant);
            }
        }
        Ok(out)
    }

    /// S038: Eagerly open (and create if needed) the per-tenant SQLite DB
    /// and run the schema migration. Converts deferred runtime errors
    /// (corrupt file, bad schema, dim mismatch) into startup errors.
    /// Idempotent.
    ///
    /// Review W1 fix: `open_conn` reads `memory_schema_meta` BEFORE the
    /// INSERT and skips the id/dim check when the table is empty. If two
    /// processes race to initialise the same empty tenant DB with
    /// different embedders, both see an empty meta, both run the INSERT,
    /// one wins, and the other returns `Ok` without noticing. Catch it
    /// here by re-reading the meta row AFTER `open_conn` returns and
    /// failing fast if what we just 'inserted' doesn't actually match
    /// our own embedder_id/dim.
    fn ensure_tenant(&self, tenant: &TenantId) -> Result<(), MemoryError> {
        let conn = self.open_conn(tenant)?;

        let (stored_id, stored_dim): (String, i64) = conn.query_row(
            "SELECT embedder_id, dim FROM memory_schema_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let stored_dim = stored_dim as usize;

        if stored_dim != self.dim {
            return Err(MemoryError::EmbedderDimensionMismatch {
                stored: stored_dim,
                configured: self.dim,
            });
        }
        if stored_id != self.embedder_id.as_str() {
            return Err(MemoryError::EmbedderMismatch {
                stored: stored_id,
                configured: self.embedder_id.as_str().to_string(),
                requires_wipe: false,
            });
        }
        Ok(())
    }
}
