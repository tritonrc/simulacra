//! `VectorIndex` trait — chunked embedding store with version-aware upsert
//! and tenant-partitioned search.
//!
//! See S037 §3.

use simulacra_types::{HitId, Locator, MemoryPath, MemoryVersion, TenantId};

use crate::embedder::EmbedderId;
use crate::error::MemoryError;

/// Maximum `retry_count` before a `memory_embed_backlog` row is considered
/// dead-lettered. Shared between the background drainer (which skips
/// processing dead-lettered rows) and the SQLite implementation of
/// `take_backlog_batch` (which filters them at the SQL layer so they are
/// invisible to the drainer). Both sides MUST agree — a single source of
/// truth avoids the drainer hot-spinning on rows the scheduler would never
/// process.
///
/// Dead-lettered rows remain in the table so operators can inspect them;
/// they just stop consuming embedder capacity.
pub const BACKLOG_MAX_RETRIES: u32 = 10;

/// A chunk staged for upsert. Carries the embedding alongside the text.
#[derive(Debug, Clone)]
pub struct IndexedChunk {
    pub chunk_index: usize,
    pub locator: Locator,
    pub text: String,
    pub embedding: Vec<f32>,
}

/// A row pulled from `memory_embed_backlog` by the backlog-draining
/// worker. Identifies a `(path, version)` needing re-embedding, with
/// the accumulated `retry_count` so callers can dead-letter after a
/// threshold.
#[derive(Debug, Clone)]
pub struct BacklogRow {
    pub path: MemoryPath,
    pub version: MemoryVersion,
    pub retry_count: u32,
}

/// Outcome of a `VectorIndex::upsert` call. The version-aware contract is
/// the core race-safety mechanism: late embedding work cannot resurrect
/// deleted or overwritten content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// The submitted version was higher than the stored version (or no
    /// stored version existed) — chunks were written.
    Applied,
    /// The stored version is >= the submitted version — no change.
    Stale,
    /// The path has been tombstoned at a version >= the submitted version
    /// — no change. The submitted upsert was racing with a delete.
    Tombstoned,
}

/// A search result returned by `VectorIndex::search`. Does NOT include a
/// `hit_id` — that is minted by the tool layer (`semantic_search`) using
/// the per-process `HitIdCache`. Wrapped in `ToolSearchHit` before being
/// returned to the agent.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: MemoryPath,
    pub chunk_index: usize,
    /// The content version this chunk was embedded from. Used by the tool
    /// layer to mint a `HitId` and by `memory_read_chunk` for the TOCTOU
    /// guard.
    pub version: MemoryVersion,
    pub locator: Locator,
    /// Snippet of length ≤ `MEMORY_SNIPPET_CHARS`. Drawn from the chunk text.
    pub snippet: String,
    /// Mathematical cosine similarity in `[-1.0, 1.0]`. Higher is more
    /// similar. NOT remapped to `[0, 1]`. See S037 §3 score contract.
    pub cosine_score: f32,
}

/// What `semantic_search` returns to the agent: a `SearchHit` wrapped with
/// a freshly minted `HitId`. The agent passes the `HitId` to
/// `memory_read_chunk` to retrieve the full chunk text (subject to a
/// TOCTOU re-check).
#[derive(Debug, Clone)]
pub struct ToolSearchHit {
    pub hit_id: HitId,
    pub path: MemoryPath,
    pub chunk_index: usize,
    pub locator: Locator,
    pub snippet: String,
    pub cosine_score: f32,
}

/// Semantic retrieval over embedded chunks, tenant-partitioned.
///
/// Implementations:
/// - **MUST** physically partition tenants (separate database files).
/// - **MUST** reject upserts whose vectors are not unit-normalized
///   (within 1e-5 of L2 norm = 1).
/// - **MUST** drop stale upserts (where stored_version >= submitted_version)
///   without modification.
/// - **MUST** filter `search` results to the provided `tenant` and `scope`
///   prefix before returning.
///
/// See S037 §3, §6.
pub trait VectorIndex: Send + Sync + 'static {
    /// Upsert chunks for a source path AT a specific version.
    ///
    /// Returns:
    /// - `Applied` if the version is higher than stored — chunks written
    /// - `Stale` if the stored version is >= submitted version — no change
    /// - `Tombstoned` if the path is deleted at version >= submitted — no change
    ///
    /// All embeddings in `chunks` MUST be unit-normalized.
    fn upsert(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
        embedder_id: &EmbedderId,
        chunks: &[IndexedChunk],
    ) -> Result<UpsertOutcome, MemoryError>;

    /// Delete all chunks for a path at or below the given tombstone version.
    /// Idempotent — calling on a missing path is not an error.
    fn delete_path(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        tombstone_version: MemoryVersion,
    ) -> Result<(), MemoryError>;

    /// Delete all chunks under a prefix. Returns the number of chunks removed.
    fn delete_prefix(&self, tenant: &TenantId, prefix: &MemoryPath) -> Result<u64, MemoryError>;

    /// Search within a scope prefix. Returns top-K chunks with scores.
    /// Constrained to `tenant` — never returns hits from other tenants.
    fn search(
        &self,
        tenant: &TenantId,
        scope: &MemoryPath,
        query_embedding: &[f32],
        embedder_id: &EmbedderId,
        k: usize,
        min_cosine: Option<f32>,
    ) -> Result<Vec<SearchHit>, MemoryError>;

    /// Return the embedder id stored in this tenant's `memory_schema_meta`.
    /// Used at engine startup to detect model changes.
    fn embedder_fingerprint(&self, tenant: &TenantId) -> Result<Option<EmbedderId>, MemoryError>;

    /// Mark all embeddings in a tenant as stale (clears `memory_vectors`,
    /// preserves `memory_chunks`). Used by `reindex_background` policy on
    /// same-dim model change.
    fn mark_tenant_stale(&self, tenant: &TenantId) -> Result<u64, MemoryError>;

    /// S037 §13: populate `memory_embed_backlog` with one row per
    /// distinct path in `memory_chunks`, stamped with the version
    /// currently in `memory_chunks`. Used by the `reindex_background`
    /// policy to hand off same-dim re-embed work. Returns the number of
    /// rows actually inserted (idempotent via `INSERT OR IGNORE` — repeat
    /// calls return 0).
    ///
    /// Intended to be called at startup, BEFORE the background embedder
    /// is spawned. Concurrent callers writing to the backlog would see
    /// `INSERT OR IGNORE` silently skip newer versions.
    fn enqueue_backlog_from_chunks(&self, _tenant: &TenantId) -> Result<u64, MemoryError> {
        Ok(0)
    }

    /// S037 §13: populate `memory_embed_backlog` with one row per path in
    /// `memory_content` that is not tombstoned (`deleted = 0`). Used by
    /// the `wipe_and_rebuild` policy after chunks have been dropped — the
    /// worker reads each row, loads the content blob, re-chunks, and
    /// embeds. Returns the number of rows actually inserted (idempotent
    /// via `INSERT OR IGNORE` on PK = path).
    ///
    /// Intended to be called at startup, BEFORE the background embedder
    /// is spawned. See `enqueue_backlog_from_chunks` for the same
    /// precondition.
    fn enqueue_backlog_from_content(&self, _tenant: &TenantId) -> Result<u64, MemoryError> {
        Ok(0)
    }

    /// S037 §13 backlog worker: load all chunks (and their embeddings,
    /// if any) for `(tenant, path, version)`. Returns empty if the
    /// coordinate has no chunk rows. Chunks written via
    /// `upsert_chunks_only` have empty `embedding` Vecs; chunks written
    /// through `upsert` have populated embeddings.
    fn load_chunks_for(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
    ) -> Result<Vec<IndexedChunk>, MemoryError> {
        Ok(Vec::new())
    }

    /// S037 §13 backlog worker: write chunks to `memory_chunks` WITHOUT
    /// writing corresponding vectors in `memory_vectors`. Used by the
    /// `wipe_and_rebuild` path: re-chunk from content, persist text rows,
    /// then embed + upsert vectors in a follow-up call.
    fn upsert_chunks_only(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
        _chunks: &[IndexedChunk],
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    /// S037 §13 backlog worker: write vectors for chunks that already
    /// exist in `memory_chunks` at `(tenant, path, version)`, matched
    /// by `chunk_index` in ascending order. `embeddings[i]` is stored
    /// for the chunk with the `i`th smallest `chunk_index`.
    ///
    /// Used by the reindex_background path: `mark_tenant_stale`
    /// cleared the vectors but left chunks in place; this method
    /// fills the vectors back in under the new embedder. Also used by
    /// the wipe_and_rebuild path after `upsert_chunks_only` has
    /// staged fresh text rows.
    ///
    /// Replaces any existing vector rows for the chunks at this
    /// coordinate (idempotent with respect to double-drain).
    ///
    /// Rejects if `embeddings.len()` does not match the number of
    /// chunks stored at `(path, version)`.
    fn write_vectors_for_chunks(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
        _embedder_id: &EmbedderId,
        _embeddings: &[Vec<f32>],
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    /// S037 §13: take up to `batch_size` backlog rows, ordered
    /// `(retry_count ASC, enqueued_at ASC)` so failed rows do not
    /// starve fresh ones. Rows remain in the table — the worker either
    /// deletes them on success (via `delete_backlog_row`) or bumps
    /// retry via `bump_backlog_retry` on failure.
    fn take_backlog_batch(
        &self,
        _tenant: &TenantId,
        _batch_size: usize,
    ) -> Result<Vec<BacklogRow>, MemoryError> {
        Ok(Vec::new())
    }

    /// S037 §13: delete a backlog row after the worker successfully
    /// re-embedded its (path, version). Idempotent — deleting a
    /// missing row is not an error.
    fn delete_backlog_row(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    /// S037 §13: increment `retry_count` and record `last_error` on a
    /// backlog row after a failed re-embed attempt. Idempotent —
    /// bumping a missing row is not an error.
    fn bump_backlog_retry(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
        _last_error: &str,
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    /// Return the number of deferred-indexing rows for a tenant.
    ///
    /// Used by observability to export `simulacra_memory_reindex_backlog`.
    /// Implementations without durable backlog tracking can return zero.
    fn backlog_count(&self, _tenant: &TenantId) -> Result<u64, MemoryError> {
        Ok(0)
    }

    /// Enumerate every tenant that this index has on-disk state for.
    ///
    /// Used by observability to drive the per-tenant backlog gauge even
    /// for tenants whose in-memory worker has been evicted or never spawned.
    /// The default impl returns an empty list, which is correct for the
    /// in-memory test fakes — they don't persist anything and have no
    /// concept of "known tenants" across process boundaries.
    fn known_tenants(&self) -> Result<Vec<TenantId>, MemoryError> {
        Ok(Vec::new())
    }

    /// Fetch a single chunk by `(tenant, path, version, chunk_index)`.
    /// Returns the locator and full chunk text, or `None` if no chunk exists
    /// at the given coordinates. Used by the `memory_read_chunk` tool to
    /// resolve a `HitId` after the TOCTOU guard has confirmed the version.
    fn get_chunk(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
        chunk_index: usize,
    ) -> Result<Option<(Locator, String)>, MemoryError>;

    /// S038: Ensure the per-tenant vector index is open, migrated, and
    /// has a schema meta row. Eager version of the lazy-open path. Must
    /// be idempotent. The SQLite implementation overrides; the default
    /// is a no-op. See S038 §Bootstrap.
    fn ensure_tenant(&self, _tenant: &TenantId) -> Result<(), MemoryError> {
        Ok(())
    }

    /// S037 §8 overflow path: stage a `(path, version)` pair in
    /// `memory_embed_backlog` when the background embedder's per-tenant
    /// dispatch channel is saturated on a `MemoryEvent::Put`.
    ///
    /// The content is already durably in `memory_content` — `MemoryStore::put`
    /// completed before the event was emitted — so the backlog drainer can
    /// pick up the row, load the content, re-chunk, and re-embed. This
    /// method only records the debt.
    ///
    /// Idempotence rules (strict-version-wins):
    /// - No row exists for `path`: insert `(path, version, now, retry_count=0,
    ///   last_error=NULL)`.
    /// - A row exists at a **lower** `version`: advance `version`, reset
    ///   `retry_count` to 0, clear `last_error`, refresh `enqueued_at`. A
    ///   newer Put supersedes the prior debt and deserves a fresh retry budget.
    /// - A row exists at the **same or a higher** `version`: no-op. We must
    ///   not reset retry_count or clear last_error for an older Put that
    ///   lost the race to a newer backlog entry — that would let a flaky
    ///   old event starve out the dead-letter signal.
    ///
    /// Default impl is a no-op so in-memory test fakes without a durable
    /// backlog table can opt in. The SQLite impl overrides with an atomic
    /// `INSERT ... ON CONFLICT(path) DO UPDATE`.
    fn enqueue_backlog_for(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
    ) -> Result<(), MemoryError> {
        Ok(())
    }
}
