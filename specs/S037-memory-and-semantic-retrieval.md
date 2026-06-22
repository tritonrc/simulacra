# S037 — Agent Memory & Semantic Retrieval

**Status:** Active
**Revision:** 3 (second GPT-5.4 adversarial review — 5 more BLOCKERs + 4 WARNINGs addressed)
**Crates:** `simulacra-memory` (new), `simulacra-vfs`, `simulacra-server`, `simulacra-tool`, `simulacra-config`, `simulacra-runtime` (hook wiring)

## Dependencies

- **S001** — VFS: memory lives as files in tenant-scoped VFS subtrees
- **S011** — Sandbox composition: retrieval is a tool, capability-gated like any other
- **S026** — Governance hooks: retrieval queries and results pass through the hook pipeline
- **S033** — Integration fabric: admin ingestion reuses the credential + mount model
- **S034** — SimulacraEngine: agents get memory access wired at construction time

Memory is **not** built on S036. S036's artifact store is task-scoped. Memory needs its own durable store keyed on `(tenant, path)` with cross-run lifetime. This spec introduces a separate `MemoryStore` trait and `MemoryStoreFs` VFS layer that live alongside (not instead of) the S036 artifact store.

## Scope

Long-term memory for agents, unified with retrieval-augmented generation over admin-ingested documents. Single subsystem, two producer paths, one tool-based retrieval interface.

**In scope:**
- `simulacra-memory` crate with `MemoryStore`, `VectorIndex`, `Embedder`, `Chunker` traits and SQLite-backed defaults
- Local-first embedding provider (pluggable; API providers deferred)
- New VFS layer `MemoryStoreFs` mounted at `/var/memory/**`, backed by `MemoryStore` (persistent across runs, distinct from `MemoryFs` in-RAM layer)
- `/var/memory/**` subtree hierarchy with well-defined scopes (self, users, org, entities, conversations, dedup)
- `/mnt/**` VFS subtree for admin-ingested documents (RAG sources)
- Background embedding worker per-tenant with versioned upserts and overflow policy
- `semantic_search` tool and `memory_read_chunk` tool exposed to agents, **opt-in** via capability
- Source-type-aware `Locator` metadata on hits (text byte range, PDF page, HTML selector, JSONL line)
- Chunking strategies (fixed-token, markdown-section, entry-level) selected by source type
- Tenant identifier validation and filesystem-safe encoding
- Path canonicalization before authorization
- Tenant partitioning at the database file level (physical isolation)
- Per-path monotonic versioning for write/rewrite/delete ordering
- Reindex policy on embedder model change
- Retention policies (per tenant, per subtree)
- Admin ingestion API for bulk document loading
- Freshness semantics: read-your-writes within a single run, eventually consistent across runs
- Hook pipeline wiring for `semantic_search` and `memory_read_chunk` tool calls
- Virtual coworker demo exercising four loops (scoped down from six — platform-level skill distillation is S038)

**Out of scope (future specs):**
- Cross-tenant pattern detection and platform-level skill extraction (S038)
- Cross-agent workflow hardening across the same tenant (S038 — includes distillation of repeated patterns into shared skills)
- API-based embedding providers — trait supports them, implementations deferred
- pgvector or external vector store backends
- Graph-structured memory (entity-relation modeling beyond flat keyed files)
- Automatic conversation summarization for context-window compaction (S019+)
- VFS read hook coverage (the existing S026 hook pipeline only covers `tool_call`, `llm`, `spawn`, `http_request`; memory relies on tool-level gating instead, see §9)
- Learned prompt template extraction
- Automatic entity extraction / named entity indexing

## Problem

Simulacra has solved short-term memory (in-context reasoning, journal, workspace files) and single-run artifacts (S036). It has no story for long-term memory — and every major enterprise loop in the vision depends on it.

Without memory:
- A virtual employee triggered on a schedule cannot remember what it did last week
- A Slack bot cannot remember prior conversations or user preferences
- A multi-day project cannot check point and resume
- Agents re-discover the same facts every run
- Entity context (customer history, deal status) must be re-fetched from scratch every time
- Idempotency cannot be enforced across retries
- Failures repeat because nothing remembers they failed
- The compounding knowledge loop from the vision document is impossible

Every run is an amnesiac. This spec fixes that.

## The unifying insight

**RAG and semantic memory are the same subsystem.** Both are "give me the top-K chunks semantically similar to this query, scoped to content I have capability to see." What differs is the producer:

- **Admin ingestion path (RAG):** docs, policies, historical data uploaded by admins, mounted read-only under `/mnt/`
- **Agent write path (memory):** notes, checkpoints, observations the agent writes under `/var/memory/`

The retrieval mechanics are identical. Building them separately would force the same engine to be built twice, with two chunking strategies, two embedding queues, two query interfaces, and two sets of governance hooks. A single subsystem with two input paths is strictly simpler and cheaper to govern.

This collapse is the single most important design decision in this spec.

## Use cases (the full enumeration)

Each row maps to one or more VFS subtrees and a typical access pattern.

| # | Use case | Subtree | Producer | Scope | Lifetime | Retrieval shape |
|---|---|---|---|---|---|---|
| 1 | Virtual employee continuity | `/var/memory/self/` | Agent | per-agent | weeks–quarters | Semantic recall keyed on role + time |
| 2 | Conversational continuity | `/var/memory/conversations/{user}/` | Agent | per-user | months | Per-thread log with recency + semantic |
| 3 | Multi-session task checkpoints | `/var/memory/tasks/{task_id}/` | Agent | per-task | days–weeks | Exact key lookup (task id) + checkpoint file |
| 4 | Facts about people | `/var/memory/users/{id}.md` | Admin or learned | per-user | stable | Exact key + semantic |
| 5 | Facts about the business | `/var/memory/org/` | Admin-curated | per-tenant | stable | Semantic lookup |
| 6 | Entity-relational memory | `/var/memory/entities/{type}/{id}.md` | Agent + integrations | per-entity | entity lifetime | Exact key + semantic on history |
| 7 | Idempotency / dedup | `/var/memory/dedup/` | Agent | per-task / per-tenant | short (hours–days) | Exact key only, no embeddings |
| 8 | Failure memory | `/var/memory/self/failures/` | Agent | per-agent | days–weeks | Semantic lookup on intended action |
| 9 | Procedural memory / hardened skills | `/var/skills/` (S033) | Hardening pipeline | per-team → per-org → marketplace | versioned forever | Name-based lookup |
| 10 | Context overflow compaction | `/var/memory/sessions/{session_id}/` | Agent loop | per-session | session + grace | Semantic recall during active session |
| 11 | RAG over admin-ingested docs | `/mnt/{source}/` | Admin ingestion | per-tenant read-only | until removed | Semantic, chunk-level |
| 12 | Cross-agent collaboration | `/var/memory/shared/` | Agent | per-team (same tenant) | short–medium | Semantic or path-based |

**Not a memory problem** (deliberately excluded):
- Governance rules and policies → `/etc/` (configuration)
- Skill invocation → `/var/skills/` with the existing skill tool (S017, S033)
- Observability state → telemetry
- Secrets → `IntegrationRegistry` credential vault (S033)

## Design axes

| Axis | Spans |
|---|---|
| **Scope** | per-agent → per-user → per-task → per-tenant → cross-tenant (future, S038) |
| **Lifetime** | session → days → months → forever (subject to retention) |
| **Write path** | agent explicit → auto-captured from trace (future) → admin-curated → pipeline-ingested |
| **Read path** | agent explicit query → auto-injected on spawn (future) → retrieved on demand |
| **Retrieval** | key-value (exact) → semantic (embeddings) → hybrid |
| **Mutability** | append-only → overwritable → governed (approval required, future) |
| **Governance** | PII-scannable, revocable, auditable — uniform with the rest of Simulacra |

## Architecture

### §1. Two layers, cleanly separated

| Layer | Trait | Responsibility |
|---|---|---|
| **Storage** | `MemoryStore` | Durable `(tenant, path) → bytes` mapping, cross-run lifetime, atomic writes, monotonic per-path version |
| **Index** | `VectorIndex` | Chunked embedding store supporting top-K semantic search with tenant + scope filtering |

The `MemoryStore` is the source of truth for content. The `VectorIndex` is a derived, tenant-partitioned search structure. If the index is wiped, the store is unaffected and the index can be rebuilt from it.

This split matters because the review flagged (correctly) that S036's task-scoped artifact store is wrong for cross-run memory. Memory needs its own durable layer.

### §2. `MemoryStore` trait

```rust
// simulacra-memory/src/store.rs

/// Durable tenant-scoped memory storage. Source of truth for /var/memory/** content.
pub trait MemoryStore: Send + Sync + 'static {
    /// Write bytes at (tenant, path). Returns the new monotonic version.
    /// Atomic: readers see either old or new bytes, never partial.
    fn put(&self, tenant: &TenantId, path: &MemoryPath, data: &[u8])
        -> Result<MemoryVersion, MemoryError>;

    /// Read bytes at (tenant, path). Returns NotFound if missing.
    /// Returns the current version alongside the bytes (for consistency checks).
    fn get(&self, tenant: &TenantId, path: &MemoryPath)
        -> Result<(Vec<u8>, MemoryVersion), MemoryError>;

    /// True if the path exists.
    fn exists(&self, tenant: &TenantId, path: &MemoryPath) -> Result<bool, MemoryError>;

    /// List entries under a prefix. Returns canonical paths + metadata only, not bytes.
    fn list_prefix(&self, tenant: &TenantId, prefix: &MemoryPath)
        -> Result<Vec<MemoryEntry>, MemoryError>;

    /// Delete a single path. Bumps the tombstone version so stale in-flight upserts
    /// in the index queue become no-ops. Returns the tombstone version.
    fn delete(&self, tenant: &TenantId, path: &MemoryPath)
        -> Result<MemoryVersion, MemoryError>;

    /// Delete everything under a prefix (retention reaper, admin bulk delete).
    fn delete_prefix(&self, tenant: &TenantId, prefix: &MemoryPath)
        -> Result<u64, MemoryError>;

    /// Subscribe to write/delete events. Returns a receiver that the background
    /// embedder consumes. Events carry the post-write version so the embedder
    /// can drop stale work.
    fn subscribe(&self) -> Result<MemoryEventReceiver, MemoryError>;
}

pub struct MemoryEntry {
    pub path: MemoryPath,
    pub size: u64,
    pub version: MemoryVersion,
    pub mtime: std::time::SystemTime,
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MemoryVersion(pub u64);

pub enum MemoryEvent {
    Put { tenant: TenantId, path: MemoryPath, version: MemoryVersion, content_hash: [u8; 32] },
    Delete { tenant: TenantId, path: MemoryPath, version: MemoryVersion },
}
```

### §3. `VectorIndex` trait

```rust
// simulacra-memory/src/index.rs

/// Semantic retrieval over embedded chunks, tenant-partitioned.
pub trait VectorIndex: Send + Sync + 'static {
    /// Upsert chunks for a source path AT a specific version. If the current
    /// stored version is >= the version provided, the upsert is a no-op (stale).
    /// This is the core race-safety mechanism: late embedding work never
    /// resurrects deleted or overwritten content.
    fn upsert(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        version: MemoryVersion,
        embedder_id: &EmbedderId,
        chunks: &[IndexedChunk],
    ) -> Result<UpsertOutcome, MemoryError>;

    /// Delete all chunks for a path at or below the given version.
    /// Version-aware deletion prevents a resurrected upsert race.
    fn delete_path(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        tombstone_version: MemoryVersion,
    ) -> Result<(), MemoryError>;

    /// Delete all chunks under a prefix (subtree removal).
    fn delete_prefix(&self, tenant: &TenantId, prefix: &MemoryPath) -> Result<u64, MemoryError>;

    /// Search within a scope prefix. Returns top-K chunks with scores.
    /// The search is constrained to `tenant` — there is no API surface that
    /// allows a query to span tenants, even by mistake.
    fn search(
        &self,
        tenant: &TenantId,
        scope: &MemoryPath,
        query_embedding: &[f32],
        embedder_id: &EmbedderId,
        k: usize,
        min_cosine: Option<f32>,
    ) -> Result<Vec<SearchHit>, MemoryError>;

    /// Return the embedder id currently recorded for a sample of rows.
    /// Used at startup to detect model-change situations.
    fn embedder_fingerprint(&self, tenant: &TenantId) -> Result<Option<EmbedderId>, MemoryError>;

    /// Reindex scaffolding: mark all chunks in a tenant as stale (clears embeddings,
    /// keeps chunk text). Used when the embedder model changes.
    fn mark_tenant_stale(&self, tenant: &TenantId) -> Result<u64, MemoryError>;
}

pub enum UpsertOutcome {
    Applied,           // version was higher, chunks written
    Stale,             // stored version >= submitted version, no change
    Tombstoned,        // path is deleted (tombstone version >= submitted), no change
}

pub struct IndexedChunk {
    pub chunk_index: usize,
    pub locator: Locator,
    pub text: String,
    pub embedding: Vec<f32>,
}

/// Returned by VectorIndex::search. Does NOT include a hit_id — that is minted
/// by the tool layer after search returns, because hit_ids require access to
/// the per-process HitId cache which the index layer doesn't own.
pub struct SearchHit {
    pub path: MemoryPath,
    pub chunk_index: usize,
    pub version: MemoryVersion,  // the content version this chunk was embedded from
    pub locator: Locator,
    pub snippet: String,         // MEMORY_SNIPPET_CHARS-capped preview
    pub cosine_score: f32,       // mathematical cosine similarity in [-1.0, 1.0]
}

/// Returned by the semantic_search tool (not the index). Wraps SearchHit with
/// a minted hit_id that the agent uses with memory_read_chunk.
pub struct ToolSearchHit {
    pub hit_id: HitId,           // signed, opaque, 5-minute TTL
    pub path: MemoryPath,
    pub chunk_index: usize,
    pub locator: Locator,
    pub snippet: String,
    pub cosine_score: f32,
}

pub struct HitId(pub String);
pub const MEMORY_SNIPPET_CHARS: usize = 320;
pub const HIT_ID_TTL_SECONDS: u64 = 300;
```

**Score contract:** `cosine_score` is the **mathematical cosine similarity** in `[-1.0, 1.0]`, where 1.0 is identical direction, 0.0 is orthogonal, -1.0 is opposite direction. Embedders produce unit-normalized vectors so the stored dot product equals cosine, but the **range is not remapped**. Typical useful thresholds:
- `min_cosine = 0.0` — "weakly related or better" (default)
- `min_cosine = 0.5` — "clearly relevant"
- `min_cosine = 0.75` — "strongly relevant"
- `min_cosine = 0.9` — "near-duplicate"

These thresholds are portable across embedders **only** if all embedders produce unit-normalized vectors (which is stated below as a trait contract). `VectorIndex` implementations assert normalization on upsert.

**Model identity:** `EmbedderId = "{model_name}@{model_version}:{dim}"`. Every stored chunk records the embedder id that produced it. A query with a different `embedder_id` from what is stored in a tenant triggers the model-change policy (§13). See §13 for the fixed-dimension constraint and the reindex path.

**Unit-vector invariant:** all embedders in `simulacra-memory` MUST return unit-normalized vectors (L2 norm = 1 ± 1e-5). Providers that produce unnormalized vectors normalize before returning from `embed`. `VectorIndex::upsert` verifies norm in debug builds and rejects malformed input in release builds. This is the only way `cosine_score` is comparable across tenants.

### §4. `Locator` — source-type-aware chunk addressing

Different source types need different coordinates. A raw `byte_range` is wrong for PDFs (binary), stripped HTML (the text doesn't exist at those offsets in the source), or JSONL (a line is the unit, not a byte range).

```rust
pub enum Locator {
    /// Plain text or Markdown — byte range is valid in the source file.
    Text { byte_start: usize, byte_end: usize },

    /// PDF — page number (1-indexed) and paragraph ordinal within the page.
    PdfPage { page: u32, paragraph: u32 },

    /// HTML — DOM path expression (CSS selector form) and byte range within
    /// the extracted text.
    HtmlSelector { selector: String, text_start: usize, text_end: usize },

    /// JSONL or NDJSON — 0-indexed line number.
    JsonlLine { line: u64 },

    /// Opaque — locator the source format understands, carried through to
    /// the caller as a hint but not interpreted by the index.
    Opaque { kind: String, payload: String },
}
```

Chunkers produce the right variant. `memory_read_chunk` (see §9) uses the locator to reconstruct full chunk content, which may differ from "read N bytes at offset X" in the source file.

### §5. Tenant identifiers and path safety

**BLOCKER 7:** raw tenant namespace interpolation into a file path is unsafe. This spec requires a canonical tenant id format validated at config load.

```rust
pub struct TenantId(String);

impl TenantId {
    /// Accepts only [a-z0-9][a-z0-9_-]{0,63}. Rejects everything else.
    pub fn parse(s: &str) -> Result<Self, TenantIdError> { ... }

    /// Safe filesystem encoding — returns the same string (it's already
    /// in the safe subset). Never contains path separators, dots, or nulls.
    pub fn as_fs_segment(&self) -> &str { &self.0 }
}
```

`TenantConfig::namespace` must be a valid `TenantId` at config load time. The config loader rejects tenants that fail the pattern. Existing tenants with non-conforming names are a migration issue, not a free pass.

**MemoryPath** is similarly constrained:

```rust
pub struct MemoryPath(String);

impl MemoryPath {
    /// Canonicalize and validate. Rejects:
    /// - paths not starting with /var/memory/ or /mnt/
    /// - any `..` component (rejected, not collapsed)
    /// - any absolute-escape attempt
    /// - trailing slashes (internal) — normalized away
    /// - null bytes, control characters
    /// - segments longer than 255 bytes
    /// - total path longer than 1024 bytes
    pub fn parse(s: &str) -> Result<Self, MemoryPathError> { ... }

    pub fn starts_with_prefix(&self, prefix: &MemoryPath) -> bool {
        // Segment-boundary prefix match. "/var/memory/self" does not match
        // "/var/memory/selfish". Only "/var/memory/self/..." or exact match.
    }
}
```

**BLOCKER 5:** the existing VFS `path.rs::normalize` collapses `..` silently. That behavior is retained for general VFS use but is **explicitly wrong for authorization**. `MemoryPath::parse` rejects `..` as a hard error. The `semantic_search` tool uses `MemoryPath::parse` on the `scope` argument before doing anything else. No canonicalization-then-check dance — the parse IS the canonicalization, and failure is an auth denial.

### §6. Storage backend: SQLite

One SQLite file per tenant at `{data_dir}/memory/{tenant_fs_segment}.db`. The filename uses the validated `TenantId::as_fs_segment`, so injection is impossible.

**Pragmas on open:**
```
PRAGMA journal_mode = WAL;           -- concurrent reads during writes
PRAGMA busy_timeout = 5000;          -- wait 5s before returning BUSY
PRAGMA synchronous = NORMAL;         -- durable but not fsync-per-write
PRAGMA foreign_keys = ON;
```

**Schema:**
```sql
-- MemoryStore tables
CREATE TABLE memory_content (
    path            TEXT PRIMARY KEY,      -- validated MemoryPath
    version         INTEGER NOT NULL,      -- monotonic, bumped on every write and delete
    content_hash    BLOB NOT NULL,         -- sha256 of bytes
    size            INTEGER NOT NULL,
    mtime_ns        INTEGER NOT NULL,
    data            BLOB NOT NULL,         -- small payloads inline; large payloads via blob ref
    deleted         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX memory_content_prefix ON memory_content(path);

-- VectorIndex tables
CREATE TABLE memory_chunks (
    chunk_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    path            TEXT NOT NULL,
    version         INTEGER NOT NULL,      -- the content version this chunk was derived from
    chunk_index     INTEGER NOT NULL,
    locator_kind    TEXT NOT NULL,         -- 'text' | 'pdf_page' | 'html_selector' | 'jsonl_line' | 'opaque'
    locator_payload TEXT NOT NULL,         -- JSON
    text            TEXT NOT NULL,
    embedder_id     TEXT NOT NULL,         -- "model_name@model_version:dim"
    UNIQUE(path, version, chunk_index)
);

CREATE INDEX memory_chunks_by_path ON memory_chunks(path);

-- sqlite-vec virtual table for ANN.
-- IMPORTANT: the `FLOAT[N]` dimension is NOT hard-coded in source — it is
-- templated at tenant-DB creation time from the configured embedder's dim.
-- The resulting DDL is interpolated as e.g. `FLOAT[384]` for MiniLM or
-- `FLOAT[1024]` for a larger model. Once the table exists, the dimension
-- is frozen for the lifetime of that tenant DB. A model swap with a
-- DIFFERENT dim cannot be handled by mark_tenant_stale alone — it requires
-- `wipe_and_rebuild`, which DROPs and recreates both `memory_vectors` and
-- `memory_chunks` with the new dimension. See §13.
CREATE VIRTUAL TABLE memory_vectors USING vec0(
    chunk_id INTEGER PRIMARY KEY,
    embedding FLOAT[{DIM}]             -- templated at DB creation
);

-- Deferred-indexing backlog: when the embedding queue overflows, writes
-- land here instead of being dropped. A reaper task periodically re-queues
-- entries when the main queue has capacity.
CREATE TABLE memory_embed_backlog (
    path          TEXT PRIMARY KEY,
    version       INTEGER NOT NULL,     -- the content version that needs embedding
    enqueued_at   INTEGER NOT NULL,     -- epoch ns
    retry_count   INTEGER NOT NULL DEFAULT 0,
    last_error    TEXT                  -- nullable; set on failed attempts
);

CREATE INDEX memory_embed_backlog_enqueued ON memory_embed_backlog(enqueued_at);

-- Tenant-level schema metadata. A single row, written at DB creation.
CREATE TABLE memory_schema_meta (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    embedder_id     TEXT NOT NULL,      -- "model_name@model_version:dim"
    dim             INTEGER NOT NULL,   -- dim frozen at creation
    created_at      INTEGER NOT NULL
);

-- Audit: who embedded which rows with which model
CREATE TABLE memory_embedder_log (
    applied_at  INTEGER NOT NULL,
    embedder_id TEXT NOT NULL,
    chunk_count INTEGER NOT NULL,
    action      TEXT NOT NULL          -- 'upsert' | 'reindex' | 'delete' | 'wipe_and_rebuild'
);
```

**Write concurrency (WARNING 4):** SQLite in WAL mode allows concurrent readers during a writer. The `MemoryStore` serializes writes per DB file (single writer, multiple readers). The background embedder is also a writer — to avoid writer contention with agent writes, the embedder batches upserts and holds the writer lock only for the duration of the batch. The `busy_timeout=5000ms` covers transient contention.

**Pick one: `sqlite-vec`.** The previous revision listed `sqlite-vec` or `sqlite-vss` as alternatives. This revision picks `sqlite-vec` exclusively. It is actively maintained, more portable, and supports fixed-dim tables which match the per-tenant embedder model.

### §7. Freshness semantics

This is the hardest contract in the spec. Three concrete guarantees:

**Guarantee 1: MemoryStore is linearizable within a tenant.** A successful `put` is immediately visible to subsequent `get` calls from any agent in the same tenant. This is just what SQLite in WAL mode gives you. The `MemoryStoreFs` VFS layer inherits this — `file_read` after `file_write` is deterministic.

**Guarantee 2: Read-your-writes within a single run, for small writes.** Within a single agent run, a write to `/var/memory/**` followed by a `semantic_search` that matches **must** return the new content, subject to the size cap below.

Mechanism — the **per-run recent writes buffer (RRWB)**:

```rust
pub struct RecentWritesBuffer {
    // Bounded ring buffer, fixed capacity.
    entries: VecDeque<BufferedWrite>,
    // Byte cap on total buffered content.
    total_bytes: usize,
}

pub struct BufferedWrite {
    path: MemoryPath,
    version: MemoryVersion,
    text: String,
    // Lazily populated by embed-on-query.
    chunks: Option<Vec<IndexedChunk>>,
}

pub const RRWB_MAX_ENTRIES: usize = 64;
pub const RRWB_MAX_BYTES_PER_ENTRY: usize = 64 * 1024;   // 64 KB
pub const RRWB_MAX_TOTAL_BYTES: usize = 1 * 1024 * 1024; // 1 MB
```

Concrete rules:

1. **Per-run scope.** The RRWB is owned by the agent's `AgentLoop` for the lifetime of a single run (one `invoke_agent` span). Not shared across runs, not shared across sibling agents in the same tenant, not shared across processes. When the run ends, the RRWB is dropped.

2. **What gets buffered.** Every successful `MemoryStoreFs::write` to `/var/memory/**` triggers a write event that hits BOTH the background embedder queue AND the RRWB. Writes to `/mnt/**` never use the RRWB (admin ingestion is not in-run).

3. **Size cap per entry.** If the write payload is `> RRWB_MAX_BYTES_PER_ENTRY` (64 KB), it is **not** buffered. The write still goes to the store and to the background embedder queue. Read-your-writes Guarantee 2 does **not** apply to oversized writes — they fall back to Guarantee 3 (eventually consistent, typically <2s). This is called out explicitly in the tool docs so agents know that a 100 MB write will not be immediately searchable.

4. **Entry count and total byte caps.** If adding a new entry would exceed `RRWB_MAX_ENTRIES` (64) or `RRWB_MAX_TOTAL_BYTES` (1 MB), the oldest entry is evicted. Evicted entries are still in the store and queue — they're just no longer in the same-run fast path. The agent's `semantic_search` after eviction sees evicted entries only when the background embedder has caught up.

5. **RRWB scoring — MVP and target.**

   **Target design (S037 end state):** embed-on-query. When `semantic_search` is called, any RRWB entries with `chunks = None` (not yet embedded) are embedded **synchronously** before the search runs. The embedder call is the same one used by the background worker; the cosine score is a real unit-vector cosine in `[-1, 1]`, directly comparable to persistent index scores. Typical latency for embedding 64 chunks is ~50–200 ms locally. The RRWB is written-to-chunks-once, not re-embedded on subsequent queries.

   **MVP implementation (Wave B):** the shipped `RecentWritesBuffer` uses case-insensitive substring matching on stored text instead of embedding. Each hit carries a synthetic relevance score derived from match density (`1 - 1/(1 + match_count)` ∈ `[0, 1)`). This is NOT a real cosine similarity and is NOT comparable to persistent index scores at the numeric level. The MVP is sufficient for demos and the virtual coworker test loop, but the tool layer MUST treat RRWB hits as a distinct category when merging with persistent hits — see point 6 below.

   **Migration plan:** the target design lands alongside the `semantic_search` tool in Wave C. At that point, RRWB's `new` constructor grows to accept an `Arc<dyn Embedder>` + a chunker selector, the `search` method becomes `async` and embeds pending entries synchronously, and the score field becomes a true cosine. The current MVP is a commitment-free stepping stone.

6. **Merge.** `semantic_search` runs the persistent index query AND the RRWB query, then merges:

   - **Dedup.** For any path that appears in both RRWB and persistent results, the RRWB hit wins (RRWB is always at the current version; persistent may be lagging). Drop the persistent hit.
   - **Score comparability (MVP).** RRWB scores and persistent scores are NOT numerically comparable in the MVP. The merge sorts by category first, then by score within each category: RRWB hits appear before persistent hits of similar relevance, reflecting that they are strictly newer. For `min_cosine` floors, the MVP applies the floor to persistent hits only — RRWB hits are always returned if they match the scope and query. This is a known approximation; the target embed-on-query design eliminates the asymmetry.
   - **Truncation.** After merging, truncate to `k` with RRWB hits prioritized when both categories are non-empty.
   - **Target design (post-MVP):** once RRWB uses real cosines, the merge is a simple unified sort on `cosine_score` descending — no category asymmetry.

7. **Cross-process caveat.** In multi-process deployments (multiple Simulacra server instances behind a load balancer), a write from process A is immediately visible only to subsequent queries within the same run on process A. A sibling agent in the same tenant on process B sees the write via Guarantee 3 (background index, typically <2s). This is acceptable because a single agent run is pinned to a single worker thread on a single process (S035 worker pool).

8. **No cross-run visibility.** A write from run X is NOT visible via RRWB to a later run Y, even on the same process. Run Y sees writes only via the persistent index (Guarantee 3). The RRWB is explicitly run-scoped, not process-scoped. This avoids stale cross-run data.

**Guarantee 3: Eventually consistent across runs, bounded by queue depth.** Writes from run A are searchable from run B (or any other run) within a bounded time.
- **Target:** p50 < 2 seconds, p99 < 30 seconds, from `put` returning to the corresponding write event being visible in `semantic_search` results from a new run
- **Measurement:** the background embedder emits `simulacra_memory_embed_lag_seconds` (time from `MemoryEvent::Put.timestamp` to successful `upsert`)
- **No bound** under queue saturation — the spec target assumes the embedding queue is not perpetually full; saturation is observable via `simulacra_memory_queue_depth` and is operator-actionable

The distinction between Guarantees 1, 2, and 3 is explicit and tested (§20 freshness assertions).

### §8. Queue overflow policy

**BLOCKER 4:** the original spec said "embedding falls behind" without specifying the overflow behavior. This revision defines it:

1. The per-tenant embedding queue is bounded (default 2048 events).
2. When the queue has room: writer enqueues the event and returns immediately.
3. When the queue is full:
   - Writer **blocks** on the queue with a 100ms bounded wait.
   - If the wait times out, the writer inserts a row into the `memory_embed_backlog` table (see §6 schema) with `(path, version, enqueued_at, retry_count=0)` and returns success to the caller.
   - A reaper task periodically sweeps `memory_embed_backlog` and re-enqueues entries when the main queue has capacity; on each failed attempt `retry_count` is incremented and `last_error` populated.
   - When an entry is successfully embedded, its row in `memory_embed_backlog` is deleted.
4. Writes to `MemoryStore` **always** succeed. Only the indexing fanout can fall behind.
5. If the embedder is permanently wedged (e.g., the model fails to load), the reaper exponentially backs off and emits `simulacra_memory_reindex_backlog{tenant}` metrics. Operators can alert.

The upshot: the store is always the source of truth. The index is best-effort-fresh with a bounded typical lag and an observable worst case. No writes are silently dropped; no deferred work is silently forgotten.

### §9. Retrieval surface: tools, not VFS reads

Two tools are introduced. Both are gated behind an **opt-in memory capability** (see §11). Neither is auto-registered for all agents.

**`semantic_search`**

```json
{
  "name": "semantic_search",
  "description": "Retrieve the top-K most relevant memory chunks matching a query, scoped to a VFS subtree the agent has read access to.",
  "parameters": {
    "query":      { "type": "string", "required": true, "maxLength": 2048 },
    "scope":      { "type": "string", "required": true, "description": "MemoryPath prefix, e.g. /var/memory/self/ or /mnt/policies/" },
    "k":          { "type": "integer", "default": 5, "max": 20 },
    "min_cosine": { "type": "number",  "default": 0.0, "max": 1.0 }
  }
}
```

Response:
```json
{
  "hits": [
    {
      "hit_id":        "hit_01HABC...",
      "path":          "/var/memory/self/notes/2026-04-03.md",
      "snippet":       "…the quarterly close requires pulling table X from BigQuery…",
      "locator":       { "kind": "text", "byte_start": 412, "byte_end": 680 },
      "cosine_score":  0.82
    }
  ]
}
```

Steps the tool runs:
1. `MemoryPath::parse(scope)` — rejects `..`, invalid prefixes, etc. Returns `400` on failure.
2. Capability check: the parsed scope must be a prefix of at least one entry in `MemoryCapability.search_scopes`. Failure returns an empty hit list with a `memory.search.out_of_scope=true` warning span, **not** an error — avoids leaking grant shape.
3. Hook pipeline `tool_call` before-phase: the hook sees the full query + scope. A hook can deny (returns empty hits + `memory.search.denied=true` span) or modify parameters.
4. Embed the query via the tenant's embedder.
5. Run persistent index search: `VectorIndex::search(tenant, scope, embedding, embedder_id, k, min_cosine)`. This returns `Vec<SearchHit>` with no `hit_id` field.
6. **Embed-on-query** for the RRWB: any RRWB entries with `chunks = None` and `path` inside the scope are embedded synchronously. Entries already embedded are reused.
7. **Merge:** RRWB-derived hits and persistent hits are merged by `(path, version)` deduplication. For equal `path`, the higher `version` wins (RRWB is always at the current version; persistent may be lagging). Sort by `cosine_score` descending, truncate to `k`, apply `min_cosine` floor.
8. Hook pipeline `tool_call` after-phase: the hook sees the merged hits and can redact snippets or remove hits entirely.
9. **Mint `HitId`s** for each surviving hit in the tool layer (not the index). For each hit, record `(tenant, path, chunk_index, version, expires_at)` in the per-process `HitIdCache` with `expires_at = now + HIT_ID_TTL_SECONDS`. Wrap each `SearchHit` as a `ToolSearchHit` with its minted `hit_id`.
10. Return the `ToolSearchHit` list. Journal and span the call with post-hook values.

**`HitIdCache` thread safety.** The cache is a single `Mutex<HashMap<HitId, CacheEntry>>` per simulacra-server process, shared across all tenants and all agent runs. It is written by `semantic_search` (one lock acquisition per call) and read by `memory_read_chunk` (one lock acquisition per call). Entries are evicted on TTL expiry by a sweeper task that runs every 60 seconds; the cache holds at most `HIT_ID_CACHE_MAX = 65536` entries across all tenants. When full, the sweeper evicts oldest-first. HitId minting uses a `ChaCha20Rng` seeded at server start; tokens are 24-byte random strings base32-encoded (192 bits of entropy, unguessable).

**`memory_read_chunk`**

```json
{
  "name": "memory_read_chunk",
  "description": "Retrieve the full text of a chunk previously returned by semantic_search.",
  "parameters": {
    "hit_id": { "type": "string", "required": true }
  }
}
```

Response (success):
```json
{
  "path":     "/var/memory/self/notes/2026-04-03.md",
  "locator":  { "kind": "text", "byte_start": 412, "byte_end": 680 },
  "content":  "…full chunk text here…"
}
```

Steps:
1. **Hit cache lookup.** Resolve `hit_id` in the `HitIdCache`.
   - Missing: return `{ error: "hit_not_found", code: 404 }` — the hit_id was never issued, is malformed, or was evicted.
   - Expired (current time > `expires_at`): return `{ error: "hit_expired", code: 404, hint: "re-run semantic_search" }`.
2. **Version consistency check (TOCTOU guard).** Using the cached `(tenant, path, version)`, query `MemoryStore` for the current version of that path.
   - Path no longer exists (tombstoned after search): return `{ error: "chunk_deleted", code: 410, path }`.
   - Current version > cached version (overwritten after search): return `{ error: "chunk_stale", code: 410, path, hint: "re-run semantic_search to get the latest content" }`.
   - Current version == cached version: proceed.
3. Hook pipeline `tool_call` before-phase sees `(path, chunk_index, version)`. A hook can deny (returns `{ error: "denied", code: 403 }` with `memory.read_chunk.denied=true` span).
4. Load the chunk from `memory_chunks` where `path = ? AND version = ? AND chunk_index = ?` and materialize full text using the locator. If the chunk row is missing despite the store having the content (an internal inconsistency — should not happen except during reindex), return `{ error: "chunk_unavailable", code: 503, hint: "reindex in progress" }`.
5. Hook pipeline `tool_call` after-phase sees the full chunk content. A hook can mutate the returned text (redact PII) before it reaches the LLM.
6. Return the final content. Journal and span the call with post-hook values.

**TTL tradeoff.** `HIT_ID_TTL_SECONDS = 300` (5 minutes). If an agent is paused waiting for approval (`ExitReason::AwaitingApproval`) for longer than 5 minutes, outstanding hit_ids expire. On resume, the agent calls `memory_read_chunk` and gets `hit_expired`. The documented recovery is: re-run `semantic_search` to get fresh hit_ids. This is acceptable because re-searching is cheap (tens of ms) and produces fresher results reflecting any writes during the pause. Agents that need to reference specific chunks across long pauses should write the chunk content to `/var/memory/sessions/{session_id}/` instead of relying on hit_ids.

**Why memory_read_chunk is the enforcement point.** This is the fix for BLOCKER 9 from review 1. S026 hooks cover `tool_call` but not VFS reads. By routing full chunk content through a tool (not through `file_read`), we get hook coverage. Agents cannot bypass the redaction by calling `file_read` on the path because:
- For `/var/memory/**` paths, `MemoryStoreFs` gates reads on `MemoryCapability.search_scopes`, not `paths_read`. A memory-disabled agent cannot read these paths.
- For `/mnt/**` paths, same applies.
- Even when a memory-enabled agent has `search_scopes` grant, `file_read` returns the raw bytes of the file — which may be a PDF or HTML with the "interesting" text buried in binary/markup. The chunks in `memory_chunks` are the extracted, searchable text, which is only accessible through `memory_read_chunk`.
- For plain-text files (e.g. `/var/memory/self/notes/*.md`), `file_read` does return readable content, and a hook that wants to gate chunk-level text should also gate `file_read` via the future VFS hook extension (out of scope here). For MVP, admins declaring `search_scopes` accept that raw `file_read` on those paths is possible.

This pair of tools replaces the "return file path, agent does `file_read`" pattern from the previous revision. The motivation is hook enforceability: S026 hooks wrap `tool_call` but not VFS reads, so chunk content must flow through a tool for redaction to work.

**Opt-in default (BLOCKER 13):** `semantic_search` and `memory_read_chunk` are NOT auto-registered by `register_builtins`. They are registered by a dedicated `register_memory_tools(registry, memory_handle)` called only when the agent type's capability explicitly enables memory (§11).

### §10. Two producer paths

**Admin ingestion (RAG):**
```
1. POST /api/v1/ingestion
   {
     "source": "hr-policies",      // must match ^[a-z0-9][a-z0-9_-]{0,62}$
     "mode":   "replace" | "merge", // defaults to "merge"
     "files":  [ ... ]              // multipart or base64
   }
2. Source name is validated. Tenant is derived from the authenticated request — there is no body or URL parameter for tenant.
3. Mode semantics:
   - "merge" (default): per-file upsert. New files added; existing files with the same path are overwritten (version bumped); files previously in /mnt/{source}/ but not in this upload are left alone.
   - "replace": before writing, the server calls `MemoryStore::delete_prefix(tenant, "/mnt/{source}/")` to remove all existing content under the subtree. Then the new files are written.
4. Files are written to the MemoryStore at /mnt/{source}/{filename}.
5. Write events flow into the background embedder via the normal path.
6. Content is searchable within the Guarantee 3 bound.
```

URL and S3 source ingestion are deferred to a follow-up spec. This cut only supports direct upload.

**Agent writes (memory):**
```
1. Agent calls file_write("/var/memory/self/notes/2026-04-06.md", content)
2. VFS layer MemoryStoreFs intercepts the write and routes to MemoryStore::put
3. MemoryStore issues a MemoryEvent::Put with the new version
4. The write returns to the agent (synchronous, <5ms typical)
5. Background embedder consumes the event, chunks, embeds, upserts
6. Next run (or the current run via Guarantee 2) can retrieve it
```

Both paths hit the same `VectorIndex::upsert` through the same background worker. There is no separate RAG pipeline.

### §11. Capability model for memory

The existing `CapabilityToken` gains a memory section:

```rust
pub struct MemoryCapability {
    /// If false, semantic_search and memory_read_chunk are not registered.
    pub enabled: bool,
    /// Prefixes the agent can search. Each prefix must be a valid MemoryPath.
    pub search_scopes: Vec<MemoryPath>,
    /// Prefixes the agent can write to. Each prefix must be a valid MemoryPath.
    pub write_scopes: Vec<MemoryPath>,
}

impl Default for MemoryCapability {
    fn default() -> Self {
        // Memory is disabled by default. Opt-in only.
        Self { enabled: false, search_scopes: vec![], write_scopes: vec![] }
    }
}
```

**BLOCKER 13 fix:** the engine's permissive fallback capability used for server-spawned agents must **not** enable memory. `MemoryCapability::default()` is disabled. Agent types that want memory declare it in config:

```toml
[agent_types.atlas.capabilities.memory]
enabled = true
search_scopes = ["/var/memory/self/", "/var/memory/entities/", "/mnt/"]
write_scopes  = ["/var/memory/self/"]
```

**Default narrowing (WARNING 5):** this revision removes the previous default-read grants for `/var/memory/users/`, `/entities/`, `/conversations/`. Nothing is readable by default. Agent types name the scopes they need.

**Tenant source (BLOCKER 6):** the `TenantId` used for every memory operation is pulled from the agent's execution context, which is the same place the capability check reads from. Sub-agents inherit from their parent. There is no tool parameter or API path that lets a caller specify a different tenant. This is enforced by the tool registration path: the memory tools close over a `Handle<{ tenant, memory }>` captured at agent construction time.

### §12. Chunking

The background embedder picks a chunker based on path and content type:

| Source | Chunker | Locator emitted |
|---|---|---|
| `/var/memory/**/*.md`, `/mnt/**/*.md` | markdown-section | `Text{byte_start, byte_end}` |
| `/var/memory/conversations/**/*.jsonl` | entry-level (one line per chunk) | `JsonlLine{line}` |
| `/var/memory/entities/**/*.md` | markdown-section | `Text{byte_start, byte_end}` |
| `/var/memory/dedup/**` | **not indexed** | — |
| `/var/memory/sessions/**` | markdown-section | `Text{byte_start, byte_end}` |
| `/mnt/**/*.pdf` | page-paragraph | `PdfPage{page, paragraph}` |
| `/mnt/**/*.html` | DOM-section (strip nav/footer) | `HtmlSelector{...}` |
| `/mnt/**/*.txt` | fixed-token (400 tokens, 50 overlap) | `Text{byte_start, byte_end}` |
| fallback | fixed-token (400 tokens, 50 overlap) | `Text{byte_start, byte_end}` |

**Dedup shape (WARNING 6):** files under `/var/memory/dedup/**` are bounded: max 1 KB per file, max 10,000 keys per tenant (enforced by `MemoryStore::put`). Exceeding the count triggers an LRU eviction of the oldest keys.

### §13. Embedder versioning and model change

**BLOCKER 12 fix.** Every chunk records its `embedder_id = "{model_name}@{model_version}:{dim}"`. At agent construction, Simulacra compares the configured embedder against the fingerprint stored in the tenant DB's `memory_schema_meta` row:

| Configured embedder | Stored embedder | Action |
|---|---|---|
| Same name, version, dim | Same | Normal operation |
| Different name or version, **same dim** | Dim match | Apply `[memory.on_model_change]` policy |
| **Different dim** | Dim mismatch | `refuse` and `reindex_background` both fail — only `wipe_and_rebuild` is safe |

Policies:
- **`"refuse"` (default):** any mismatch fails startup loudly with `MemoryError::EmbedderMismatch { stored, configured, requires_wipe }`. Operator must run migration.
- **`"reindex_background"`:** **only valid for same-dim changes.** Calls `VectorIndex::mark_tenant_stale` which clears embeddings (not chunks) in `memory_vectors`. Background embedder re-embeds from `memory_chunks.text` using the new embedder. During reindex, `semantic_search` continues to work against stale embeddings and returns a `memory.search.reindexing=true` span attribute. Different-dim under this policy returns `MemoryError::EmbedderDimensionMismatch` and refuses.
- **`"wipe_and_rebuild"`:** valid for any mismatch. Drops and recreates `memory_vectors` (with the new dim templated in) and `memory_chunks`. Rebuilds chunks and embeddings from `memory_content`. Produces a search gap until rebuild completes; writes during the gap accumulate in `memory_content` and are picked up by the rebuild.

**Dimension is frozen per tenant DB at creation.** The `{DIM}` template in the `memory_vectors` DDL is substituted from the configured embedder at first tenant-DB creation. Once the virtual table exists, `sqlite-vec` does not support in-place dimension changes. Only `wipe_and_rebuild` can change it.

Stored embedder id lives in `memory_schema_meta` (single-row table), not inferred from chunk-level fingerprints. Chunk-level `embedder_id` is kept for audit and for catching accidental mixed-embedder corruption (which should be impossible, but defense in depth).

### §14. VFS layer: `MemoryStoreFs`

A new VFS layer in `simulacra-vfs` that wraps an inner VFS and intercepts `/var/memory/**` and `/mnt/**` paths, routing them to `MemoryStore` instead of the inner VFS.

```rust
pub struct MemoryStoreFs<V: VirtualFs> {
    inner: V,
    tenant: TenantId,
    store: Arc<dyn MemoryStore>,
    /// The capability snapshot captured at construction time. The layer
    /// enforces search_scopes and write_scopes on every operation; it does
    /// NOT defer to the agent's generic paths_read/paths_write.
    capability: MemoryCapability,
}
```

**BLOCKER 1 fix — MemoryStoreFs is conditionally installed AND self-gating.** Two layers of defense:

1. **Conditional installation.** `SimulacraEngine::spawn_task` only wraps the VFS stack with `MemoryStoreFs` when `agent_type_config.capabilities.memory.enabled == true`. When disabled, the layer is absent entirely. Reads/writes to `/var/memory/**` or `/mnt/**` from a memory-disabled agent fall through to the inner VFS (`MailboxFs → MemoryFs`), which does not have those paths and returns `VfsError::NotFound`.

2. **Self-gating when installed.** When installed, `MemoryStoreFs` enforces the `MemoryCapability` scopes on every operation **before** touching the store. The generic `paths_read`/`paths_write` from `CapabilityToken` do NOT apply to `/var/memory/**` and `/mnt/**` — memory paths are gated exclusively by `MemoryCapability.search_scopes` (for reads) and `MemoryCapability.write_scopes` (for writes). This matters because the permissive server-default `paths_read = "/**"` would otherwise grant read on memory paths.

Operation behavior when `MemoryStoreFs` is installed:

| Op | Path | Behavior |
|---|---|---|
| `write` | `/var/memory/{p}` in a `write_scope` | `store.put(tenant, path, data)` + emit `MemoryEvent::Put` |
| `write` | `/var/memory/{p}` NOT in any `write_scope` | `PermissionDenied` |
| `write` | `/mnt/**` | `PermissionDenied` — agent cannot write to admin-ingested subtrees regardless of capability (admin API is the only writer) |
| `read` | `/var/memory/{p}` or `/mnt/{p}` in a `search_scope` | `store.get(tenant, path)` |
| `read` | `/var/memory/{p}` or `/mnt/{p}` NOT in any `search_scope` | `PermissionDenied` |
| `list_dir` | prefix in a `search_scope` | `store.list_prefix(tenant, prefix)` |
| `list_dir` | prefix NOT in any `search_scope` | `PermissionDenied` |
| `remove` | `/var/memory/{p}` in a `write_scope` | `store.delete(tenant, path)` |
| `remove` | `/var/memory/{p}` NOT in any `write_scope` | `PermissionDenied` |
| `remove` | `/mnt/**` | `PermissionDenied` |
| any | non-memory path | Pass through to inner VFS unchanged |
| `snapshot`/`restore` | any | Delegate to inner (memory content lives in durable store, not session snapshots) |

**Note on the "file_read bypass" concern from review 2 BLOCKER 1:** agents with `paths_read = "/**"` cannot bypass `MemoryCapability` by calling `file_read("/var/memory/self/note.md")` directly, because when `MemoryStoreFs` is installed, the layer intercepts the read and checks `search_scopes` — not `paths_read`. When `MemoryStoreFs` is NOT installed (memory disabled), `/var/memory/**` doesn't exist in the VFS stack at all, so `file_read` returns `NotFound`. Either way, memory is gated by `MemoryCapability`, not by the generic capability token.

**Engine fallback must keep memory disabled.** The permissive fallback capability that `SimulacraEngine` synthesizes for agents without explicit config (see `build_capability_token`) must set `memory: MemoryCapability::default()` — which is `{ enabled: false, search_scopes: [], write_scopes: [] }`. This is an explicit assertion in §20.

The agent VFS stack updated to include this layer:
```
ProcFs
  └─ ServiceFs
      └─ MemoryStoreFs                 ← new, backed by MemoryStore
          └─ MailboxFs                  ← S036, backed by ArtifactStore
              └─ MemoryFs (in-RAM)      ← existing workspace
```

**BLOCKER 8 fix:** the previous revision claimed "tenant VFS root plumbing is already in place." That was wrong. `SimulacraEngine` currently constructs a fresh in-memory VFS per task, never uses `TenantConfig.vfs_root`, and has no tenant-scoped storage path. This spec adds the missing plumbing:
- `SimulacraEngine` holds `Arc<dyn MemoryStore>` (parallel to `Arc<dyn ArtifactStore>` from S036)
- Per-task VFS construction wraps the stack with `MemoryStoreFs::new(inner, tenant_id, memory_store)`
- The tenant id comes from the resolved `TenantConfig.namespace`, parsed into `TenantId` at config load

### §15. Tenant isolation invariants

Stated as hard invariants, each with a corresponding test in §18:

1. **Physical DB isolation.** Each tenant has its own SQLite file. Cross-tenant search is impossible because the wrong file is never opened.
2. **Tenant source is context, not input.** The `TenantId` used for every memory operation is captured at agent construction time from `TenantConfig.namespace`. No tool parameter accepts a tenant override.
3. **Sub-agents inherit.** Child agents spawned via the supervisor inherit the parent's `TenantId`; there is no code path to spawn a child in a different tenant.
4. **Admin ingestion is tenant-scoped.** The `POST /api/v1/ingestion` endpoint extracts the tenant from the authenticated request (same mechanism as `POST /api/v1/tasks/create`). There is no path parameter for tenant.
5. **Tenant validation at config load.** Malformed tenant ids fail config parsing. Non-conforming existing tenants fail at load and require explicit migration.
6. **Path validation rejects traversal.** `MemoryPath::parse` rejects `..`, absolute escapes, and control characters. No canonicalization-then-check pattern.

### §16. Governance wiring

The `semantic_search` and `memory_read_chunk` tools flow through the existing S026 hook pipeline via `tool_call` hooks. Specifically:

- **Before-phase `tool_call` hook** sees the raw query (`semantic_search`) or the raw hit_id (`memory_read_chunk`). A hook can modify params, deny, or pass.
- **After-phase `tool_call` hook** sees the result (hits or chunk content). A hook can mutate the result payload (redact snippets, drop hits, mask PII) before it reaches the LLM.
- **Journal entry** is written before the tool returns, capturing the final (post-hook) result for audit.
- **Span + metrics** are emitted before the tool returns; the span attributes are populated post-hook.

**BLOCKER 10 fix — log ordering.** The `memory.query` and `memory.hit_paths` span attributes are populated from the **post-hook** result, not the raw query. This prevents DLP bypass via trace logs. The telemetry assertion is: a hook that redacts "customer_ssn" in results must cause span attributes to contain the redacted value, not the raw.

**BLOCKER 9 fix — hook coverage.** Hook coverage for chunk content is achieved by routing chunk reads through `memory_read_chunk` (a `tool_call`, which IS covered by S026) rather than through `file_read` (a VFS read, which is NOT covered by S026). The hook pipeline is unchanged; the tool design routes sensitive reads through the covered path.

**Deny/error UX (WARNING 7).** A denied `semantic_search` returns `{ hits: [] }` with a span `memory.search.denied = true`. It does not error. This prevents information leakage about the denial reason and keeps the agent loop robust — the agent sees "no results" and can continue. The denial reason is in the span and the journal, visible to operators.

**Runtime wiring (WARNING 8).** The simulacra-server agent loop currently passes `None` for hooks in some paths. This spec adds an assertion that the memory tools receive the tenant's `HookPipeline` at registration time, and that a denial by a configured hook is visible in a test.

### §17. Retention

Configurable per-subtree retention with a background reaper:

```toml
[memory.retention]
"/var/memory/dedup/**"          = "7d"
"/var/memory/sessions/**"       = "30d"
"/var/memory/self/**"           = "forever"
"/var/memory/conversations/**"  = "180d"
"/var/memory/tasks/**"          = "90d"
"/var/memory/entities/**"       = "365d"
"/var/memory/org/**"            = "forever"
"/mnt/**"                       = "forever"
```

- Reaper runs per tenant, configurable interval (default 1h).
- Reaper iterates the `memory_content` table (cheap index scan) and selects rows older than the subtree's retention.
- Deletion calls `MemoryStore::delete` (bumps version, triggers index deletion via the normal event path).
- The reaper holds no global locks; one reaper task per tenant.
- On large tenants (>1M entries), the reaper paginates to bound per-batch work.

### §18. Observability

Every memory operation produces a span parented to the agent's `invoke_agent` trace:

- `memory_store_put` — tenant, path, size, new_version, inline_or_blob
- `memory_store_get` — tenant, path, hit (bool), version
- `memory_store_delete` — tenant, path, tombstone_version
- `memory_search` — tenant, scope, k, hit_count, top_score, denied (bool), embed_latency_ms, index_latency_ms
- `memory_read_chunk` — tenant, hit_id_valid, path, chunk_index
- `memory_embedder_batch` — tenant, model, batch_size, duration_ms
- `memory_index_upsert` — tenant, path, version, outcome (Applied/Stale/Tombstoned), chunks_written
- `memory_reaper_sweep` — tenant, subtree, deleted_count, duration_ms

Metrics (OTel, per-tenant unless noted):
- `simulacra_memory_writes_total{tenant, subtree}`
- `simulacra_memory_searches_total{tenant, scope, denied}`
- `simulacra_memory_embed_latency_seconds{model}` — histogram
- `simulacra_memory_embed_lag_seconds{tenant}` — histogram, the time from put to successful upsert
- `simulacra_memory_index_size_bytes{tenant}`
- `simulacra_memory_queue_depth{tenant}` — embedding backlog gauge
- `simulacra_memory_reindex_backlog{tenant}` — gauge of rows in `memory_embed_backlog`
- `simulacra_memory_overflow_total{kind}` — counter of events that fell through the per-tenant queue overflow path, keyed on `kind` = `put` \| `delete`; rising value signals embedder capacity is undersized before the reindex-backlog gauge does
- `simulacra_memory_hit_id_cache_size{}` — gauge of active hit_ids across the server

All query strings and hit paths in spans and logs are **post-hook**, not raw.

## §19. MVP cut

Minimal shippable slice. Each item has at least one assertion in §20.

**1. `simulacra-memory` crate and types**
- `MemoryStore`, `VectorIndex`, `Embedder`, `Chunker` traits
- `TenantId`, `MemoryPath`, `MemoryVersion`, `HitId`, `Locator` types with validation
- `MemoryError` with explicit variants
- `MemoryCapability` type used by `CapabilityToken`

**2. `SqliteMemoryStore`**
- Single SQLite file per tenant
- WAL mode, busy_timeout
- Atomic put with monotonic version
- Subscribe API for write/delete events

**3. `SqliteVectorIndex`**
- `sqlite-vec` backend (no alternative)
- Version-aware upsert (Applied / Stale / Tombstoned outcomes)
- Scope-prefix filtered search
- `mark_tenant_stale` for model-change reindex

**4. Default `Embedder`**
- Local-first (ONNX runtime + small default model, or Ollama endpoint)
- `EmbedderId` carries model name + version + dim
- Normalizes output vectors to unit length so cosine similarity is in [0, 1]

**5. `Chunker` registry with MVP implementations**
- Markdown-section chunker → `Text` locator
- Fixed-token-window chunker (fallback) → `Text` locator
- PDF and HTML chunkers deferred to follow-up (fallback chunker covers ingestion in the meantime)
- JSONL entry chunker → `JsonlLine` locator

**6. `MemoryStoreFs` VFS layer**
- Intercepts `/var/memory/**` reads, writes, list_dir, remove
- Intercepts `/mnt/**` reads (writes from agents are denied)
- Delegates non-memory paths to inner VFS
- Integrated into `SimulacraEngine::spawn_task` VFS stack

**7. Background embedder**
- Per-tenant tokio task subscribed to `MemoryStore` events
- Bounded queue (2048 default), overflow policy per §8
- Version-aware upsert; stale events dropped
- `memory_embed_backlog` reaper for deferred work

**8. `semantic_search` and `memory_read_chunk` tools**
- Registered only when `MemoryCapability.enabled == true`
- Parse + canonicalize scope, reject `..`
- Capability check against `search_scopes`
- `HitId` cache with 5-minute TTL
- Hook pipeline wiring (before + after) for both tools
- Cosine score contract; `min_cosine` in `[0.0, 1.0]`

**9. Read-your-writes buffer**
- In-process per-run buffer of recent writes (bounded)
- Consulted by `semantic_search` and merged with persistent results
- Dropped at run end

**10. Embedder model-change policy**
- `EmbedderId` fingerprint check at agent construction
- `[memory.on_model_change]` config: `refuse` (default), `reindex_background`, `wipe_and_rebuild`
- Operator-visible span + log on mismatch

**11. Retention reaper**
- Per-tenant background task
- Reads `[memory.retention]` config
- Deletes expired content and triggers index removal via the normal event path
- Paginated

**12. Admin ingestion API (minimal)**
- `POST /api/v1/ingestion` — multipart/base64 upload only
- Tenant from authenticated request
- Writes to `/mnt/{source}/` via `MemoryStore`
- No URL/S3 fetch in this cut

**13. Tenant id validation at config load**
- `TenantId::parse` applied to every `TenantConfig.namespace`
- Failing tenants reject config load with a clear error

**14. SimulacraEngine integration**
- Engine holds `Arc<dyn MemoryStore>`
- VFS stack includes `MemoryStoreFs`
- Hook pipeline passed to memory tools at registration

## The virtual coworker demo — MVP proving ground (scoped down from 6 loops to 4)

**BLOCKER 15 fix:** the previous demo's cross-coworker distillation loop and pattern-detection loop pulled S038 work into S037. Both are removed from this spec's demo. What remains proves four loops that only need S037 capabilities.

Three coworkers, one tenant, shared memory where appropriate:

| Coworker | Role | Trigger | Reads | Writes |
|---|---|---|---|---|
| **Atlas** | Finance analyst | Cron daily 09:00 | `/mnt/financial-docs/`, toy SaaS deal data | `/var/memory/self/`, `/var/memory/entities/deals/` |
| **Sol** | Customer success | Webhook on new ticket | `/var/memory/entities/customers/`, `/mnt/product-docs/` | `/var/memory/self/failures/`, `/var/memory/entities/customers/` |
| **Nova** | Ops generalist | @mention in Slack | `/var/memory/org/`, `/var/memory/entities/customers/`, `/mnt/hr-policies/` | `/var/memory/conversations/` |

### The four loops that must work

1. **Individual learning (Atlas).** Day 1: Atlas discovers which tables to pull and writes a note to `/var/memory/self/notes/bigquery-schema.md`. Day 2: Atlas searches its own notes first, finds the answer, skips the rediscovery step.

2. **Failure avoidance (Sol).** Sol tries to post to `#announcements`, gets 403. Writes to `/var/memory/self/failures/slack-403-announcements.md`. Next invocation with a similar action checks failure memory first and picks a different channel.

3. **Cross-agent entity memory, same tenant (Sol → Nova).** Sol handles a ticket for customer X and writes observations to `/var/memory/entities/customers/X.md`. A week later, Nova is asked in Slack about customer X. Nova searches `/var/memory/entities/customers/` and retrieves Sol's notes. Sol and Nova never communicate directly — the memory subtree is the communication channel.

4. **RAG over admin-ingested docs (Nova).** Admin uploads HR policies to `/mnt/hr-policies/` via the ingestion API. Nova is asked "what's our PTO policy?" and searches `/mnt/hr-policies/`, returning an answer with a citation.

### What "done" looks like

- A new coworker type is a config entry plus persona prompt — no custom Rust code
- Memory growth is bounded by retention and observable via metrics
- Search latency meets the §20 performance targets
- All four loops run end-to-end in a demo script with real LLM calls
- Cross-tenant isolation is provable via assertion tests in §20

### Explicitly NOT in this demo (deferred to S038)

- Platform-level pattern detection across coworkers
- Shared-skill proposal and approval
- Measurable day-1-vs-day-30 cost improvement (this is a workflow-hardening outcome, not a memory outcome)
- Cross-coworker skill distillation

## §20. Assertions

Assertions originally carried a category prefix: `[U]` unit test, `[I]` integration test, `[B]` benchmark, `[A]` real-LLM acceptance. After a PM audit (2026-04-19), assertions now use the standard `- [x]` / `- [ ]` format. Category intent is preserved in the assertion text where relevant; the raw prefixes have been dropped so `SPECS.md` can count coverage the same way as the other specs.

### Types and validation

- [x] `TenantId::parse` accepts `^[a-z0-9][a-z0-9_-]{0,63}$` and rejects everything else
- [x] `TenantConfig::namespace` is parsed into `TenantId` before memory wiring; invalid tenants skip `MemoryStoreFs` install with a warning (runtime validation in `SimulacraEngine::spawn_task`)
- [x] `MemoryPath::parse` rejects paths containing `..`, null bytes, control chars, segments > 255 bytes, total > 1024 bytes
- [x] `MemoryPath::parse` rejects paths not starting with `/var/memory/` or `/mnt/`
- [x] `MemoryPath::starts_with_prefix` is segment-boundary: `/var/memory/selfish` does NOT match prefix `/var/memory/self`
- [x] `HitId` tokens are 24 bytes of CSPRNG-generated randomness, base32-encoded
- [x] `HIT_ID_TTL_SECONDS == 300` and the constant is used consistently
- [x] `Locator` serializes/deserializes losslessly for all five variants (exercised via `sqlite_vector_index` chunk roundtrip)
- [x] `SearchHit` has NO `hit_id` field; `ToolSearchHit` wraps it with one

### `MemoryStore` (SQLite backend)

- [x] `put` is atomic: concurrent readers see old or new bytes, never partial
- [x] `put` returns a monotonically increasing version per path
- [x] `delete` bumps the version (tombstone) and subsequent `get` returns NotFound
- [x] Deletion event is visible to the subscription channel
- [x] `list_prefix` returns canonical paths, sizes, versions, mtimes, hashes
- [x] Two tenants with the same path do not share data (physical DB isolation)
- [x] SQLite file is placed at `{data_dir}/memory/{tenant_fs_segment}.db`
- [x] Non-conforming tenant id is impossible (validated at parse time) so filename injection cannot occur
- [x] Concurrent writes to the same path serialize correctly; last writer wins with monotonic version
- [x] WAL mode is enabled and `busy_timeout` is ≥ 5000ms
- [x] `memory_schema_meta` has a single row recording the embedder_id and dim at DB creation
- [x] Dim stored in `memory_schema_meta` matches the dim in the `memory_vectors` virtual table DDL

### `VectorIndex` (SQLite + sqlite-vec)

- [x] `upsert` with `version > stored_version` returns `Applied` and replaces chunks
- [x] `upsert` with `version < stored_version` returns `Stale` and leaves the index unchanged
- [x] `upsert` after `delete_path` with a version ≤ tombstone returns `Tombstoned`
- [x] `delete_path` removes all chunks for a path and records the tombstone version
- [x] `delete_prefix` removes all chunks under the prefix
- [x] `search` is constrained to the provided tenant (no cross-tenant leakage, proven by test with two tenants)
- [x] `search` filters by scope prefix and respects `k` and `min_cosine`
- [x] `search` results are sorted by descending `cosine_score`
- [x] `cosine_score` is in `[-1.0, 1.0]` (mathematical cosine, NOT remapped)
- [x] `SearchHit` does NOT contain a `hit_id` field (hit_ids are minted by the tool layer)
- [x] `embedder_fingerprint` returns the stored embedder_id from `memory_schema_meta`
- [x] `mark_tenant_stale` clears embeddings in `memory_vectors` but preserves rows in `memory_chunks`
- [x] `upsert` rejects vectors whose L2 norm deviates from 1.0 by more than 1e-5 (release builds)

### `Embedder`

- [x] Default local embedder loads at startup without network
- [x] `embed` returns unit-normalized vectors (L2 norm = 1 ± 1e-5)
- [x] `model_id` returns a stable identifier that survives process restart
- [x] Batch embedding preserves input order in output
- [x] Embedder is swappable via config; swap triggers the model-change path in §13

### Chunkers

- [x] Markdown-section chunker splits on `#`/`##`/`###` and preserves section boundaries
- [x] Fixed-token-window chunker emits 400-token chunks with 50-token overlap
- [x] JSONL entry chunker emits one chunk per non-empty line
- [x] Chunkers produce the correct `Locator` variant
- [x] Unknown content type falls back to fixed-token chunker
- [x] Content under `/var/memory/dedup/**` is not chunked or indexed

### Background embedder

- [x] Consumes `MemoryEvent::Put` and upserts chunks into the index
- [x] Consumes `MemoryEvent::Delete` and deletes chunks from the index
- [x] Stale events (lower version than current) are dropped with a span
- [x] Bounded queue (default 2048); enqueue blocks for up to 100ms under pressure
- [x] Overflow path writes rows to `memory_embed_backlog` (NOT a column on `memory_chunks`); reaper re-queues them — `background.rs::dispatch_event` calls `VectorIndex::enqueue_backlog_for` on Put overflow (strict-version-wins upsert in `sqlite_index.rs`) and calls `VectorIndex::delete_path` synchronously on Delete overflow so stale chunks can't remain searchable. Drainer (`backlog_drain_loop`) consumes staged rows; `take_backlog_batch` filters rows at `retry_count >= BACKLOG_MAX_RETRIES=10` so dead-lettered rows can't hot-spin the drainer. Tests: `crates/simulacra-memory/tests/background_embedder.rs::{overflow_put_writes_to_memory_embed_backlog, overflow_backlog_row_is_drained_and_search_finds_content, overflow_delete_removes_search_hits_even_under_saturation, enqueue_backlog_for_advances_version_and_resets_retry, backlog_dead_letter_row_is_not_retried_past_max_retries, take_backlog_batch_excludes_dead_lettered_rows}`.
- [x] `simulacra_memory_embed_lag_seconds` metric is emitted with p50 and p99 buckets (histogram with bucket bounds `[0.01, 0.05, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]`)
- [x] Permanent embedder failure (model load) is visible as a metric and an alertable state (`simulacra_memory_embedder_load_failures_total` counter with low-cardinality `reason` attribute; `simulacra-memory::record_embedder_load_failure` invoked from `simulacra-cli::run_booted` on `DefaultEmbedder::load_default` error; `crates/simulacra-memory/tests/memory_metrics.rs::embedder_load_failure_counter_increments_by_reason`)

### Freshness

- [x] **Guarantee 1 — MemoryStore linearizable:** a successful `put` is immediately visible to subsequent `get` in the same tenant (WAL ordering)
- [x] **Guarantee 2 — Read-your-writes within a run, small writes:** agent writes `/var/memory/self/x.md` (≤64 KB), then `semantic_search` with a matching query in the same run, and x.md appears in the hits
- [x] **Guarantee 2 — Oversized writes fall back to Guarantee 3:** agent writes a 200 KB file; same-run `semantic_search` may not see it (not guaranteed); next-run search sees it within the Guarantee 3 bound
- [x] **Guarantee 2 — RRWB capacity:** after 65 writes in a single run, the 1st write is evicted from the RRWB; search no longer sees it via read-your-writes, but sees it once the background embedder catches up
- [x] **Guarantee 2 — Cross-run isolation:** a write from run X is NOT visible to a later run Y via RRWB; run Y sees it only via persistent index
- [x] **Guarantee 2 — Embed-on-query:** when `semantic_search` fires with pending RRWB entries, those entries are embedded synchronously before the search returns; the query may add up to ~200ms on a large batch
- [ ] **Guarantee 3 — Eventually consistent across runs:** p50 < 2s, p99 < 30s from `put` returning to hit visibility in a new agent run on the same tenant, with the background embedder at steady state — benchmark harness not written
- [x] **Write-rewrite visibility:** agent writes v1, then v2; search (next run) returns only v2's content
- [x] **Write-delete visibility:** agent writes v1, then deletes; search returns zero hits, and zero stale chunks remain in `memory_chunks`
- [x] **Stale upsert safety:** a queued upsert arriving after a delete does not resurrect the content (race test with forced ordering)

### `semantic_search` tool

- [x] Registered only when `MemoryCapability.enabled == true`
- [x] Not present in the default builtin tool set
- [x] `scope` is parsed via `MemoryPath::parse`; malformed scopes return `400`
- [x] `scope` must be a prefix of at least one entry in `search_scopes`; outside scopes return `{hits: []}` (span attribute `memory.search.out_of_scope` not yet surfaced — see observability gaps below)
- [x] `k` respects the max of 20 (schema `maximum: 20` + `.clamp(1, MAX_K)` at initial parse and post-hook reparse; `crates/simulacra-server/tests/memory_tools.rs::semantic_search_k_schema_advertises_maximum_of_20`, `semantic_search_k_over_max_returns_at_most_20_hits`)
- [x] `min_cosine` respects the range `[-1.0, 1.0]` (default `0.0`) (schema `minimum: -1.0, maximum: 1.0` + `.clamp(MIN_COSINE_LOWER, MIN_COSINE_UPPER)`; `crates/simulacra-server/tests/memory_tools.rs::semantic_search_min_cosine_schema_advertises_unit_range`, `semantic_search_min_cosine_above_1_is_clamped_and_returns_empty`, `semantic_search_min_cosine_below_neg1_is_clamped_and_permits_hits`)
- [x] Result hits are `ToolSearchHit` with `hit_id`, `path`, `snippet`, `locator`, `cosine_score`
- [x] Snippet is capped at `MEMORY_SNIPPET_CHARS` (320) — enforced in tool code
- [x] Hook pipeline `tool_call` before-phase sees the query; deny returns `{hits: []}` with `memory.search.denied=true`
- [x] Hook pipeline after-phase sees the hits; redaction mutates the returned result
- [x] Span attributes reflect post-hook values (no DLP bypass via trace logs)
- [x] HitIds are minted by the tool, not the index; 24-byte random base32, CSPRNG-sourced
- [x] HitIdCache is process-wide, bounded to 65536 entries, evicts oldest-first when full

### `memory_read_chunk` tool

- [x] Registered only when `MemoryCapability.enabled == true`
- [x] Missing `hit_id` in cache returns `{ error: "hit_not_found", code: 404 }`
- [x] Expired `hit_id` returns `{ error: "hit_expired", code: 404 }` with re-search hint
- [x] **TOCTOU guard:** if the underlying path's current version > cached version, returns `{ error: "chunk_stale", code: 410 }` with re-search hint
- [x] **TOCTOU guard:** if the underlying path has been deleted since the hit was issued, returns `{ error: "chunk_deleted", code: 410 }`
- [x] Hook pipeline `tool_call` before-phase sees the resolved `(path, chunk_index, version)`
- [x] Hook pipeline after-phase can redact the returned chunk content
- [x] Span attributes are post-hook
- [x] A chunk cannot be read for a `hit_id` issued more than `HIT_ID_TTL_SECONDS` (300s) ago

### `MemoryStoreFs` VFS layer

- [x] **Conditional install:** when `MemoryCapability.enabled == false`, `SimulacraEngine::spawn_task` does NOT wrap the VFS stack with `MemoryStoreFs`; `/var/memory/**` and `/mnt/**` return `NotFound` via the inner VFS
- [x] **Self-gating when installed:** read to `/var/memory/**` outside `search_scopes` returns `PermissionDenied`, even if agent's `paths_read = "/**"`
- [x] Write to `/var/memory/**` outside `write_scopes` returns `PermissionDenied`
- [x] Write to `/mnt/**` from agent context always returns `PermissionDenied` regardless of capability
- [x] Read to `/mnt/**` inside `search_scopes` succeeds
- [x] `list_dir("/var/memory/users")` (inside search_scope) enumerates via `store.list_prefix`
- [x] `remove("/var/memory/self/x.md")` (inside write_scope) calls `store.delete`
- [x] Non-memory paths delegate to the inner VFS unchanged
- [x] `snapshot` and `restore` delegate to the inner VFS
- [x] Stack order when installed: `ProcFs → ServiceFs → MemoryStoreFs → MailboxFs → MemoryFs(inner)`
- [x] Stack order when disabled: `ProcFs → ServiceFs → MailboxFs → MemoryFs(inner)` — `MemoryStoreFs` is absent

### Capability model

- [x] `MemoryCapability::default()` is `{ enabled: false, search_scopes: [], write_scopes: [] }`
- [x] `SimulacraEngine::build_capability_token` fallback for untyped agents does NOT enable memory
- [x] Attempting to register memory tools for an agent with `enabled: false` is a no-op
- [x] Agent type config can grant specific `search_scopes` and `write_scopes`
- [x] A write to a path outside `write_scopes` is rejected at the `MemoryStoreFs` layer even when `paths_write = "/**"`
- [x] A read to a path outside `search_scopes` is rejected at the `MemoryStoreFs` layer even when `paths_read = "/**"`
- [x] A search with a scope outside `search_scopes` returns `{hits: []}` with a warning span

### Tenant isolation (hard invariants)

- [x] **Physical isolation:** Tenant A's search never returns Tenant B's content, even if constructed via the low-level `VectorIndex::search` API
- [x] **No tenant parameter on tools:** `semantic_search` and `memory_read_chunk` have no tenant parameter; the tenant is captured at tool registration from the agent execution context
- [x] **Sub-agent inheritance:** A child agent spawned by the supervisor inherits the parent's tenant; there is no code path to specify a different tenant for a child
- [x] **Admin ingestion:** the ingestion endpoint uses the authenticated request's tenant; there is no URL or body parameter for tenant
- [x] **Filesystem layout:** the SQLite file for tenant A is at `{data_dir}/memory/{tenant_a}.db` and for tenant B at `{data_dir}/memory/{tenant_b}.db`; no shared file

### Embedder model change

- [x] Same embedder: normal startup, no reindex
- [x] Same dim, different name/version, `on_model_change = refuse`: startup fails with `EmbedderMismatch`
- [x] Same dim, different name/version, `on_model_change = reindex_background`: `mark_tenant_stale` called, background embedder re-embeds (impl: `simulacra_memory::apply_policy` → `mark_tenant_stale` + `enqueue_backlog_from_chunks` + `set_embedder_id_at`, test: `crates/simulacra-memory/tests/model_change_reindex.rs::same_dim_reindex_background_re_embeds_existing_chunks`)
- [x] Different dim, `on_model_change = refuse`: startup fails with `EmbedderDimensionMismatch`
- [x] Different dim, `on_model_change = reindex_background`: startup fails with `EmbedderDimensionMismatch` (reindex cannot change dim)
- [x] Different dim, `on_model_change = wipe_and_rebuild`: `memory_vectors` and `memory_chunks` are dropped and recreated with new dim; content is re-chunked and re-embedded from `memory_content` (impl: `SqliteVectorIndex::wipe_and_reopen` + backlog worker, test: `crates/simulacra-memory/tests/model_change_reindex.rs::different_dim_wipe_and_rebuild_rebuilds_from_content`)
- [x] During reindex, `semantic_search` still works but returns `memory.search.reindexing=true` span attribute (impl: `SemanticSearchTool::invoke` reads `VectorIndex::backlog_count`, test: `crates/simulacra-server/tests/memory_hook_integration.rs::semantic_search_records_reindexing_true_when_backlog_nonempty`)
- [x] `memory_embedder_log` table records every bulk reindex and wipe-and-rebuild

### Retention

- [x] `[memory.retention]` is parsed and applied per subtree
- [x] Expired content is deleted from `MemoryStore` AND the index
- [x] Reaper runs at the configured interval (default 1h)
- [x] Reaper holds no global locks; tenant A's reaper cannot block tenant B
- [x] Paginated sweeps bound per-batch work for tenants with many entries
- [x] Deletion spans are journaled (one `memory_reaper_sweep` span per subtree with `tenant`, `subtree`, `deleted_count`, `duration_ms`)

> `MemoryRetentionConfig` (simulacra-config) → `RetentionReaperConfig` (simulacra-memory) wiring lives in `simulacra-cli::retention_config_to_reaper`. The reaper is spawned in `run_booted` alongside the `BackgroundEmbedder` and shut down before it. Per-tenant workers use independent `tokio::spawn` tasks (no global sweep lock). `MemoryStore::delete` is paired with `VectorIndex::delete_path` per path; index-delete failures are tracked in `ReaperStats::index_failures` rather than silently masked. Test coverage lives at `crates/simulacra-memory/tests/retention_reaper.rs` (11 tests). Backlog reaper for failed embedder enqueues remains an acknowledged follow-up.

### Admin ingestion API

- [x] `POST /api/v1/ingestion` accepts multipart / base64 uploads
- [x] `source` is validated (`^[a-z0-9][a-z0-9_-]{0,62}$`); invalid returns 400
- [x] Files land under `/mnt/{source}/` in the authenticated tenant's `MemoryStore`
- [x] `mode = "merge"` (default) upserts per file; existing unspecified files preserved
- [x] `mode = "replace"` calls `delete_prefix("/mnt/{source}/")` before writing
- [x] Ingestion reuses the background embedder (no separate pipeline)
- [x] Ingestion is auditable via task-like event stream — `POST /api/v1/ingestion/stream` returns `text/event-stream` with events sharing the same envelope as `task_events` (`/api/v1/tasks/:task_id/events`): each payload carries `event`, `ingestion_id` (per-stream UUID), and a monotonic `seq`. Sequence: `ingestion.started` → (`ingestion.cleared` for replace mode) → per-file `ingestion.written` → `ingestion.completed`, or a terminal `ingestion.error` on per-file failure. Auth/tenant/source/mode/base64/path validation happens before the stream opens so those errors surface as synchronous HTTP responses, not SSE error events on a 200 stream. A client disconnect mid-stream does NOT abort the ingest — the worker runs to completion and a terminal log records what landed. Tests: `crates/simulacra-server/tests/memory_ingestion.rs::{ingestion_stream_emits_per_file_progress_events, ingestion_stream_replace_mode_emits_cleared_event_before_writes, ingestion_stream_emits_error_event_on_invalid_base64, ingestion_stream_rejects_unauthenticated_before_opening_stream, ingestion_stream_returns_404_when_memory_disabled}`

### Hook integration

- [x] A hook that denies `semantic_search` causes the tool to return `{hits: []}` and a `memory.search.denied=true` span
- [x] A hook that redacts a snippet propagates the redaction to the span attribute
- [x] A hook that drops a hit causes the hit not to appear in the result or any post-result log
- [x] The `tool_call` hook pipeline is wired to the memory tools in `SimulacraEngine::spawn_task` (not `None`)
- [x] A hook denial on `memory_read_chunk` returns `{ error: "denied", code: 403 }`

> Memory tools own their `tool_call` hook lifecycle via the `Tool::handles_own_hooks()` opt-out added in `simulacra-types` — the generic `ToolRegistry` wrapper is skipped so deny maps to `{hits: []}` / `{error:"denied", code:403}` instead of a generic error. Post-hook span attributes (`memory.query`, `memory.hit_paths`, `memory.search.denied`, `memory.read_chunk.denied`) prevent DLP bypass via traces. See `crates/simulacra-server/tests/memory_hook_integration.rs` for 17 covering tests.

### Observability

- [x] Every memory operation produces a span (tool-layer `memory_search` / `memory_read_chunk` spans in `simulacra-tool/src/memory.rs`; index + store layers use `tracing` spans)
- [x] `simulacra_memory_embed_lag_seconds` is exported with p50/p95/p99 (histogram with buckets `[0.01, 0.05, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]`, `tenant` attribute)
- [x] `simulacra_memory_queue_depth` is exported per tenant (ObservableGauge reading the BackgroundEmbedder's per-tenant mpsc channel capacity)
- [x] `simulacra_memory_reindex_backlog` is exported per tenant (ObservableGauge via `VectorIndex::backlog_count`)
- [x] `simulacra_memory_overflow_total` is exported with `kind` attribute (`put` | `delete`) — counter incremented from `BackgroundEmbedder::dispatch_event`'s overflow branch via `simulacra_memory::metrics::record_queue_overflow`; test `crates/simulacra-memory/tests/memory_metrics.rs::overflow_counter_increments_by_kind_under_saturation`
- [x] Span attributes for query and hits are post-hook — `memory.query` + `memory.hit_paths` recorded after the after-phase hook in `SemanticSearchTool::call`

### Performance (measurable targets)

- [ ] **Search latency:** on a synthetic corpus of **100k chunks per tenant**, p99 `semantic_search` < 100ms, measured on `c5.large`-class hardware (2 vCPU, 4GB RAM) with local embedding
- [ ] **Embed lag:** on a synthetic write workload of **10 writes/sec**, p99 `simulacra_memory_embed_lag_seconds` < 30s
- [ ] **Write latency:** p99 `MemoryStore::put` < 5ms for payloads ≤ 16 KB
- [ ] **Reaper sweep:** on **1M entries per tenant**, full sweep completes within 10 minutes without blocking writers

> No `benches/` directory exists yet under `crates/simulacra-memory`. All four performance targets need a dedicated criterion or custom-harness bench before being marked done.

### Virtual coworker demo (4 loops, not 6)

- [x] Three coworker agent types (Atlas, Sol, Nova) defined in example config with `memory.enabled = true` and appropriate scopes — `crates/simulacra-server/examples/s037_virtual_coworkers.rs` now defines Atlas (search+write `/var/memory/self`), Sol (`/var/memory/entities`), Nova (search `/var/memory/entities` + `/var/memory/conversations`, write `/var/memory/conversations`)
- [x] Loop 1 (individual learning): Atlas day-1 writes a note; Atlas day-2 search returns it on a matching query — validated end-to-end with real Claude Sonnet 4.6 in `crates/simulacra-server/examples/s037_virtual_coworkers.rs` (Loop 1 section). Day-1 Atlas writes `/var/memory/self/insights/backup-null-timestamps.md`; day-2 Atlas (fresh task, no LLM context carry-over) uses `semantic_search` + `memory_read_chunk` to recall the finding and writes a cited summary to `/proc/mailbox/atlas-backup-summary.md`. All assertions passed: day-1 completed, file indexed, day-2 completed, artifact mentions "null" + "backup" + cites the memory path.
- [x] Loop 2 (failure avoidance): Sol writes a failure, next invocation checks failure memory and picks a different action — validated end-to-end in `crates/simulacra-server/examples/s037_virtual_coworkers.rs` (Loop 2 section). Attempt 1 Sol records a failed approach (socket timeout on SDK upload hang) to `/var/memory/entities/failures/globex-sdk-upload.md`; attempt 2 Sol (fresh task) searches the failures subtree, reads the top hit, and writes a Markdown proposal to `/proc/mailbox/globex-next-approach.md` that acknowledges the socket-timeout failure and proposes a different approach (chunked/multipart/pre-signed/pool/resumption). All assertions passed including the citation to the failure note.
- [x] Loop 3 (cross-agent entity memory): Sol writes to `/var/memory/entities/customers/X.md`; Nova searches and retrieves it — covered by `s037_coworkers_demo.rs`
- [x] Loop 4 (RAG): admin ingests `/mnt/hr-policies/`; Nova answers a policy question with a citation — validated end-to-end in `crates/simulacra-server/examples/s037_virtual_coworkers.rs` (Loop 4 section). Admin `POST /api/v1/ingestion` seeds three policy markdowns (pto, remote, expenses) under `/mnt/hr-policies/`; Nova (with `/mnt/hr-policies` added to `memory.search_scopes`) answers an employee question about 4-day remote work via `semantic_search` + `memory_read_chunk` and writes to `/proc/mailbox/employee-remote-work-answer.md`. Assertions passed: ingestion wrote 3 files, Nova's artifact quotes the 3-day limit, answers "no" to the 4-day ask, and cites `/mnt/hr-policies/remote.md`.
- [x] Demo runs end-to-end against a real LLM in a reproducible script (Loop 3 only)
- [x] Demo is committed as `examples/s037_virtual_coworkers.rs` — renamed from the prior `s037_coworkers_demo.rs` in the same commit that added the Atlas persona

## Out of scope

- **Cross-tenant pattern detection and platform-level skill extraction** — S038
- **Cross-agent skill distillation within a tenant** — S038 (same-tenant workflow hardening that observes patterns across Atlas/Sol/Nova and proposes shared skills is future work; S037 stops at same-tenant memory sharing via entity subtrees)
- **API-based embedding providers** — trait supports them, implementations deferred
- **pgvector or external vector stores** — trait allows it, implementation deferred
- **Graph memory / entity-relation modeling beyond flat keyed files** — future
- **Conversation summarization for context-window compaction** — agent loop concern (S019+)
- **Learned prompt template extraction** — future
- **VFS read hook coverage** — S026 hooks cover `tool_call` but not VFS reads; memory relies on tool-level gating via `memory_read_chunk` instead of extending hook coverage
- **URL and S3 source ingestion** — the MVP ingestion API is upload-only; fetch-based sources are a follow-up
- **PDF and HTML chunkers** — the trait supports them via the `Locator` enum, but MVP ships with markdown, fixed-token, and JSONL chunkers only
- **Human-in-the-loop memory curation UI** — dashboard concern

## Open questions (to resolve during implementation)

1. Should `hit_id` be signed with a per-process key, or is an unguessable random token sufficient? (Tradeoff: signed tokens survive restart; random ones don't but are simpler and 5 minutes is short enough that a lost cache is acceptable.)
2. Inline blob threshold for `memory_content.data` — at what size should we spill to a separate blob store or use SQLite's incremental BLOB I/O?
3. Reindex backpressure: should the reaper re-queue stale work at a capped rate to avoid starving live writes?
4. Should `MemoryStore::list_prefix` support pagination for very large prefixes, or is a single unbounded call acceptable for MVP?
5. Per-tenant embedder model pool: the MVP uses a single shared model across all tenants (WARNING 3); do we need a per-tenant override for enterprises with data-residency constraints?
