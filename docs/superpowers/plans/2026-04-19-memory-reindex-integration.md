# Memory Reindex Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship S037 assertions 1140, 1143, and 1144 — the end-to-end reindex integration path (config knob → startup policy dispatch → backlog-driven re-embed worker → reindexing span attr).

**Architecture:** Add `on_model_change` to `MemoryConfig` with three variants (`refuse` default / `reindex_background` / `wipe_and_rebuild`). At startup, catch `EmbedderMismatch`, dispatch per policy: `reindex_background` calls `mark_tenant_stale` then populates `memory_embed_backlog` with existing `(path, version)` rows; `wipe_and_rebuild` drops+recreates `memory_vectors` (new dim) and `memory_chunks`, then populates the backlog from `memory_content`. A new per-tenant backlog-draining worker inside `BackgroundEmbedder` pulls rows, re-chunks from content if chunks are missing, embeds, upserts, and deletes. `SemanticSearchTool` records `memory.search.reindexing = backlog_count(tenant) > 0` on its span.

**Tech Stack:** Rust, tokio, rusqlite, sqlite-vec, OpenTelemetry, `tracing`.

**Design doc:** `docs/superpowers/specs/2026-04-19-memory-reindex-integration-design.md`

**Spec:** `specs/S037-memory-and-semantic-retrieval.md` (assertions 1140, 1143, 1144)

---

## Task 1: `OnModelChange` config enum

**Files:**
- Modify: `crates/simulacra-config/src/lib.rs`

- [ ] **Step 1: Write the failing round-trip test**

Append at the bottom of the `#[cfg(test)] mod tests` block in `crates/simulacra-config/src/lib.rs`:

```rust
// S037: on_model_change round-trip, all three variants plus default.
#[test]
fn memory_on_model_change_round_trips_all_variants() {
    for (literal, expected) in [
        ("refuse", OnModelChange::Refuse),
        ("reindex_background", OnModelChange::ReindexBackground),
        ("wipe_and_rebuild", OnModelChange::WipeAndRebuild),
    ] {
        let toml_str = format!(
            r#"dir = "./.mem"
on_model_change = "{literal}"
"#
        );
        let cfg: MemoryConfig = toml::from_str(&toml_str).expect("parses");
        assert_eq!(cfg.on_model_change, expected);
        let serialized = toml::to_string(&cfg).expect("serializes");
        let reparsed: MemoryConfig = toml::from_str(&serialized).expect("reparses");
        assert_eq!(reparsed.on_model_change, expected);
    }
}

#[test]
fn memory_on_model_change_defaults_to_refuse_when_omitted() {
    let cfg: MemoryConfig =
        toml::from_str(r#"dir = "./.mem""#).expect("parses");
    assert_eq!(cfg.on_model_change, OnModelChange::Refuse);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p simulacra-config memory_on_model_change 2>&1 | tail -20
```

Expected: compile error — `OnModelChange` is not in scope; `MemoryConfig` has no `on_model_change` field.

- [ ] **Step 3: Add the enum and field**

Add, just below the `MemoryConfig` struct in `crates/simulacra-config/src/lib.rs`:

```rust
/// S037 §13: policy applied when the configured embedder does not match
/// the embedder recorded in this tenant's `memory_schema_meta`. Default
/// `refuse` preserves the "surface the mismatch" behavior; the other
/// variants dispatch into the runtime's reindex pathway.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnModelChange {
    /// Startup fails with `EmbedderMismatch` / `EmbedderDimensionMismatch`.
    #[default]
    Refuse,
    /// Same-dim mismatch: clear `memory_vectors`, preserve `memory_chunks`,
    /// re-embed in the background. Different-dim under this policy is
    /// rejected with `EmbedderDimensionMismatch`.
    ReindexBackground,
    /// Any dim: drop+recreate `memory_vectors` (new dim) and
    /// `memory_chunks`; re-chunk from `memory_content` and re-embed in
    /// the background.
    WipeAndRebuild,
}
```

In the `MemoryConfig` struct, add the field (after `retention`):

```rust
    #[serde(default)]
    pub on_model_change: OnModelChange,
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p simulacra-config memory_on_model_change 2>&1 | tail -5
```

Expected: `2 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-config/src/lib.rs
git commit -m "feat(simulacra-config): add OnModelChange to MemoryConfig [S037]"
```

---

## Task 2: `enqueue_backlog_from_chunks` trait method

**Files:**
- Modify: `crates/simulacra-memory/src/index.rs` (trait)
- Modify: `crates/simulacra-memory/src/sqlite_index.rs` (impl)
- Modify: `crates/simulacra-memory/tests/sqlite_vector_index.rs` (test)

- [ ] **Step 1: Write the failing test**

Append to `crates/simulacra-memory/tests/sqlite_vector_index.rs`:

```rust
// S037 1140: reindex_background populates memory_embed_backlog with
// one (path, version) row per distinct chunk coordinate.
#[test]
fn enqueue_backlog_from_chunks_writes_one_row_per_path_version() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let embedder_id = EmbedderId::new("fake@v1:4").unwrap();
    let idx = SqliteVectorIndex::new(dir.path(), embedder_id.clone(), 4).unwrap();

    // Seed two paths, each with two chunks.
    let path_a = MemoryPath::parse("/var/memory/a.md").unwrap();
    let path_b = MemoryPath::parse("/var/memory/b.md").unwrap();
    let chunks = vec![
        IndexedChunk {
            chunk_index: 0,
            locator: Locator::Text { start: 0, end: 4 },
            text: "aaaa".into(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
        },
        IndexedChunk {
            chunk_index: 1,
            locator: Locator::Text { start: 4, end: 8 },
            text: "bbbb".into(),
            embedding: vec![0.0, 1.0, 0.0, 0.0],
        },
    ];
    idx.upsert(&tenant, &path_a, MemoryVersion(1), &embedder_id, &chunks).unwrap();
    idx.upsert(&tenant, &path_b, MemoryVersion(1), &embedder_id, &chunks).unwrap();

    let enqueued = idx.enqueue_backlog_from_chunks(&tenant).unwrap();
    assert_eq!(enqueued, 2, "one row per distinct (path, version)");
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 2);
}

// S037: calling enqueue_backlog_from_chunks twice in a row is idempotent.
#[test]
fn enqueue_backlog_from_chunks_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let embedder_id = EmbedderId::new("fake@v1:4").unwrap();
    let idx = SqliteVectorIndex::new(dir.path(), embedder_id.clone(), 4).unwrap();
    let path = MemoryPath::parse("/var/memory/a.md").unwrap();
    let chunk = IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text { start: 0, end: 4 },
        text: "aaaa".into(),
        embedding: vec![1.0, 0.0, 0.0, 0.0],
    };
    idx.upsert(&tenant, &path, MemoryVersion(1), &embedder_id, std::slice::from_ref(&chunk)).unwrap();

    idx.enqueue_backlog_from_chunks(&tenant).unwrap();
    idx.enqueue_backlog_from_chunks(&tenant).unwrap();
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index enqueue_backlog_from_chunks 2>&1 | tail -10
```

Expected: compile error — method not found on `SqliteVectorIndex`.

- [ ] **Step 3: Add the trait method**

In `crates/simulacra-memory/src/index.rs`, add inside the `VectorIndex` trait (after `mark_tenant_stale`):

```rust
    /// S037 §13: populate `memory_embed_backlog` with one row per
    /// distinct `(path, version)` currently in `memory_chunks`. Used by
    /// the `reindex_background` policy to hand off same-dim re-embed
    /// work to the background worker. Returns the number of rows
    /// inserted. Idempotent via `INSERT OR IGNORE` on the PK.
    fn enqueue_backlog_from_chunks(&self, _tenant: &TenantId) -> Result<u64, MemoryError> {
        Ok(0)
    }
```

- [ ] **Step 4: Implement on `SqliteVectorIndex`**

In `crates/simulacra-memory/src/sqlite_index.rs`, add inside `impl VectorIndex for SqliteVectorIndex` (alongside `mark_tenant_stale`):

```rust
    fn enqueue_backlog_from_chunks(&self, tenant: &TenantId) -> Result<u64, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO memory_embed_backlog (path, version, enqueued_at)
             SELECT DISTINCT path, version, ?1 FROM memory_chunks",
            params![now_ns()],
        )?;
        tx.commit()?;
        Ok(inserted as u64)
    }
```

- [ ] **Step 5: Run test to verify it passes**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index enqueue_backlog_from_chunks 2>&1 | tail -5
```

Expected: `2 passed`.

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-memory/src/index.rs crates/simulacra-memory/src/sqlite_index.rs crates/simulacra-memory/tests/sqlite_vector_index.rs
git commit -m "feat(simulacra-memory): add VectorIndex::enqueue_backlog_from_chunks [S037]"
```

---

## Task 3: `enqueue_backlog_from_content` trait method

**Files:**
- Modify: `crates/simulacra-memory/src/index.rs` (trait)
- Modify: `crates/simulacra-memory/src/sqlite_index.rs` (impl)
- Modify: `crates/simulacra-memory/tests/sqlite_vector_index.rs` (test)

Note: `memory_content` is owned by `SqliteMemoryStore`, not `SqliteVectorIndex`. Both tables live in the same DB file (the implementations coexist via `CREATE TABLE IF NOT EXISTS`), so the index can read `memory_content` via the same `open_conn` handle.

- [ ] **Step 1: Write the failing test**

Append to `crates/simulacra-memory/tests/sqlite_vector_index.rs`:

```rust
// S037 1143: wipe_and_rebuild enqueues one backlog row per memory_content path.
#[test]
fn enqueue_backlog_from_content_writes_one_row_per_path() {
    use simulacra_memory::{MemoryStore, SqliteMemoryStore};
    use simulacra_types::{MemoryPath, MemoryVersion};
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let embedder_id = EmbedderId::new("fake@v1:4").unwrap();

    // Seed content via SqliteMemoryStore (sibling table in same DB file).
    let store = SqliteMemoryStore::new(dir.path()).unwrap();
    store.put(&tenant, &MemoryPath::parse("/var/memory/a.md").unwrap(), MemoryVersion(1), b"aaaa").unwrap();
    store.put(&tenant, &MemoryPath::parse("/var/memory/b.md").unwrap(), MemoryVersion(1), b"bbbb").unwrap();

    let idx = SqliteVectorIndex::new(dir.path(), embedder_id, 4).unwrap();
    let enqueued = idx.enqueue_backlog_from_content(&tenant).unwrap();
    assert_eq!(enqueued, 2);
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 2);
}
```

Adjust the `SqliteMemoryStore::new` / `put` calls to match the actual public API if they differ — verify with `grep -n 'pub fn' crates/simulacra-memory/src/sqlite_store.rs` before writing.

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index enqueue_backlog_from_content 2>&1 | tail -10
```

Expected: compile error — method not found.

- [ ] **Step 3: Add the trait method**

In `crates/simulacra-memory/src/index.rs`, inside `VectorIndex` (alongside `enqueue_backlog_from_chunks`):

```rust
    /// S037 §13: populate `memory_embed_backlog` with one row per
    /// `(path, version)` in `memory_content` that is not tombstoned.
    /// Used by `wipe_and_rebuild` after dropping `memory_chunks` — the
    /// worker pulls each row, reads the content blob, re-chunks, and
    /// embeds. Returns the number of rows inserted. Idempotent.
    fn enqueue_backlog_from_content(&self, _tenant: &TenantId) -> Result<u64, MemoryError> {
        Ok(0)
    }
```

- [ ] **Step 4: Implement on `SqliteVectorIndex`**

In `crates/simulacra-memory/src/sqlite_index.rs`:

```rust
    fn enqueue_backlog_from_content(&self, tenant: &TenantId) -> Result<u64, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let mut conn = self.open_conn(tenant)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO memory_embed_backlog (path, version, enqueued_at)
             SELECT path, version, ?1 FROM memory_content WHERE deleted = 0",
            params![now_ns()],
        )?;
        tx.commit()?;
        Ok(inserted as u64)
    }
```

- [ ] **Step 5: Run test to verify it passes**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index enqueue_backlog_from_content 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-memory/src/index.rs crates/simulacra-memory/src/sqlite_index.rs crates/simulacra-memory/tests/sqlite_vector_index.rs
git commit -m "feat(simulacra-memory): add VectorIndex::enqueue_backlog_from_content [S037]"
```

---

## Task 4: `wipe_vectors_and_chunks` trait method

**Files:**
- Modify: `crates/simulacra-memory/src/index.rs` (trait)
- Modify: `crates/simulacra-memory/src/sqlite_index.rs` (impl)
- Modify: `crates/simulacra-memory/tests/sqlite_vector_index.rs` (test)

- [ ] **Step 1: Write the failing test**

Append to `crates/simulacra-memory/tests/sqlite_vector_index.rs`:

```rust
// S037 1143: wipe_and_rebuild drops and recreates vectors+chunks with new dim.
#[test]
fn wipe_vectors_and_chunks_drops_and_recreates_with_new_dim() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let old_id = EmbedderId::new("fake@v1:4").unwrap();
    let new_id = EmbedderId::new("fake@v2:8").unwrap();

    // Seed chunks with dim 4.
    let idx = SqliteVectorIndex::new(dir.path(), old_id.clone(), 4).unwrap();
    let path = MemoryPath::parse("/var/memory/a.md").unwrap();
    let chunk = IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text { start: 0, end: 4 },
        text: "aaaa".into(),
        embedding: vec![1.0, 0.0, 0.0, 0.0],
    };
    idx.upsert(&tenant, &path, MemoryVersion(1), &old_id, std::slice::from_ref(&chunk)).unwrap();
    drop(idx);

    // Re-open with dim 8, call wipe_vectors_and_chunks (NOT the constructor;
    // we expect the method to drop+recreate the virtual table).
    // To construct an index for a mismatched-dim tenant, we use the private
    // SqliteVectorIndex variant that skips the embedder-mismatch check;
    // the trait method is what fixes the mismatch.
    let cleared = SqliteVectorIndex::wipe_and_reopen(dir.path(), &tenant, new_id.clone(), 8)
        .expect("wipe succeeds");
    assert_eq!(cleared, 1, "one chunk cleared");

    let idx = SqliteVectorIndex::new(dir.path(), new_id.clone(), 8).unwrap();
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 0, "backlog untouched by wipe itself");
    assert_eq!(idx.embedder_fingerprint(&tenant).unwrap().as_ref(), Some(&new_id));
}
```

Implementation note: the test calls a helper `SqliteVectorIndex::wipe_and_reopen` because the trait's `wipe_vectors_and_chunks` is awkward to exercise directly when the constructor itself rejects a dim mismatch. The helper wraps: open a raw `rusqlite::Connection` that skips schema validation, invoke the wipe DDL, update `memory_schema_meta`, return the cleared count.

Alternative, simpler approach: expose `wipe_vectors_and_chunks` as an associated function (not a method on `&self`) that opens its own connection, since the use case is always "before constructing a reconciled index".

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index wipe_vectors_and_chunks 2>&1 | tail -10
```

Expected: compile error.

- [ ] **Step 3: Implement `wipe_and_reopen` associated function**

In `crates/simulacra-memory/src/sqlite_index.rs`, add (inside `impl SqliteVectorIndex`, *not* inside the trait impl):

```rust
    /// S037 §13 wipe_and_rebuild: drop `memory_vectors` (virtual) and
    /// `memory_chunks`, recreate with the new embedder's dim, and
    /// rewrite `memory_schema_meta` to the new embedder. Intended to
    /// run BEFORE constructing a reconciled `SqliteVectorIndex` for
    /// the new embedder. Returns the count of cleared chunk rows for
    /// audit logging.
    ///
    /// This is an associated function (not a method) because the
    /// caller cannot construct an `&self` for a mismatched-dim tenant
    /// — the constructor returns `EmbedderDimensionMismatch` first.
    pub fn wipe_and_reopen(
        root: &std::path::Path,
        tenant: &TenantId,
        new_embedder_id: EmbedderId,
        new_dim: usize,
    ) -> Result<u64, MemoryError> {
        // Resolve the tenant DB path using the same scheme as open_conn.
        let path = tenant_db_path(root, tenant);  // adjust to match existing helper
        std::fs::create_dir_all(path.parent().expect("tenant path has parent"))?;
        let conn = Connection::open(&path)?;
        // Load sqlite-vec extension — same as normal open_conn.
        unsafe { sqlite_vec::sqlite3_vec_init(conn.handle(), std::ptr::null_mut(), std::ptr::null_mut()) };

        let cleared: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_chunks",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);

        conn.execute_batch(
            "BEGIN IMMEDIATE;
             DROP TABLE IF EXISTS memory_vectors;
             DROP TABLE IF EXISTS memory_chunks;",
        )?;

        // Recreate memory_chunks identically to ensure_schema.
        conn.execute_batch(
            r#"
            CREATE TABLE memory_chunks (
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
            CREATE INDEX memory_chunks_by_path ON memory_chunks(path);
            "#,
        )?;

        // Recreate the templated virtual table with the new dim.
        let vec_ddl = format!(
            "CREATE VIRTUAL TABLE memory_vectors USING vec0(\n\
             \x20   chunk_id INTEGER PRIMARY KEY,\n\
             \x20   embedding FLOAT[{dim}]\n\
             );",
            dim = new_dim
        );
        conn.execute(&vec_ddl, [])?;

        // Update schema meta to the new embedder.
        conn.execute(
            "UPDATE memory_schema_meta
                SET embedder_id = ?1, dim = ?2
              WHERE id = 1",
            params![new_embedder_id.as_str(), new_dim as i64],
        )?;
        conn.execute("COMMIT", [])?;

        Ok(cleared as u64)
    }
```

Check the existing `open_conn` code to get the exact helper names (`tenant_db_path`, the sqlite-vec init call, `Connection::open_with_flags`, etc.) and match them.

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index wipe_vectors_and_chunks 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-memory/src/sqlite_index.rs crates/simulacra-memory/tests/sqlite_vector_index.rs
git commit -m "feat(simulacra-memory): add SqliteVectorIndex::wipe_and_reopen [S037]"
```

---

## Task 5: `set_embedder_id_at` associated function

**Files:**
- Modify: `crates/simulacra-memory/src/sqlite_index.rs` (impl)
- Modify: `crates/simulacra-memory/tests/sqlite_vector_index.rs` (test)

- [ ] **Step 1: Write the failing test**

Append to `crates/simulacra-memory/tests/sqlite_vector_index.rs`:

```rust
// S037 1140: same-dim reindex updates the stored embedder_id so a later
// constructor with the new embedder passes the fingerprint check.
#[test]
fn set_embedder_id_at_updates_schema_meta() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let old_id = EmbedderId::new("fake@v1:4").unwrap();
    let new_id = EmbedderId::new("fake@v2:4").unwrap();

    let idx = SqliteVectorIndex::new(dir.path(), old_id.clone(), 4).unwrap();
    let path = MemoryPath::parse("/var/memory/a.md").unwrap();
    let chunk = IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text { start: 0, end: 4 },
        text: "aaaa".into(),
        embedding: vec![1.0, 0.0, 0.0, 0.0],
    };
    idx.upsert(&tenant, &path, MemoryVersion(1), &old_id, std::slice::from_ref(&chunk)).unwrap();
    drop(idx);

    SqliteVectorIndex::set_embedder_id_at(dir.path(), &tenant, &new_id).unwrap();

    // Reopening with the new embedder now succeeds.
    let idx = SqliteVectorIndex::new(dir.path(), new_id.clone(), 4).unwrap();
    assert_eq!(idx.embedder_fingerprint(&tenant).unwrap().as_ref(), Some(&new_id));
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index set_embedder_id 2>&1 | tail -10
```

Expected: compile error.

- [ ] **Step 3: Implement as an associated function on `SqliteVectorIndex`**

Use an associated function (not a trait method) for the same reason as `wipe_and_reopen`: the caller cannot construct an `&SqliteVectorIndex` when the fingerprint is mismatched — the constructor refuses. In `crates/simulacra-memory/src/sqlite_index.rs`, add inside `impl SqliteVectorIndex`:

```rust
    /// S037 §13 same-dim reindex: update `memory_schema_meta.embedder_id`
    /// in the tenant's DB without going through the fingerprint-
    /// validating constructor. Intended to run AFTER the caller has
    /// staged re-embed work via `mark_tenant_stale_at` +
    /// `enqueue_backlog_from_chunks_at`.
    pub fn set_embedder_id_at(
        root: &std::path::Path,
        tenant: &TenantId,
        embedder_id: &EmbedderId,
    ) -> Result<(), MemoryError> {
        let path = tenant_db_path(root, tenant);
        let mut conn = Connection::open(&path)?;
        unsafe {
            sqlite_vec::sqlite3_vec_init(
                conn.handle(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE memory_schema_meta SET embedder_id = ?1 WHERE id = 1",
            params![embedder_id.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }
```

Check the existing `open_conn` code for the exact sqlite-vec init helper, transaction behavior, and any other setup, and mirror them. If a `tenant_db_path` helper does not exist, extract the path-resolution logic from `open_conn` into a shared helper in the same task (keep it private to the module).

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index set_embedder_id_at 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-memory/src/sqlite_index.rs crates/simulacra-memory/tests/sqlite_vector_index.rs
git commit -m "feat(simulacra-memory): add set_embedder_id_at for same-dim reindex [S037]"
```

---

## Task 6: Backlog worker primitives (`take_backlog_batch`, `delete_backlog_row`, `bump_backlog_retry`)

**Files:**
- Modify: `crates/simulacra-memory/src/index.rs` (trait + new `BacklogRow` struct)
- Modify: `crates/simulacra-memory/src/sqlite_index.rs` (impl)
- Modify: `crates/simulacra-memory/tests/sqlite_vector_index.rs` (test)

- [ ] **Step 1: Write the failing test**

Append to `crates/simulacra-memory/tests/sqlite_vector_index.rs`:

```rust
// S037: backlog worker primitives — oldest-first batch, delete, bump retry.
#[test]
fn take_backlog_batch_returns_rows_oldest_first_then_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let embedder_id = EmbedderId::new("fake@v1:4").unwrap();
    let idx = SqliteVectorIndex::new(dir.path(), embedder_id.clone(), 4).unwrap();

    // Seed three backlog rows by upserting chunks then enqueuing.
    for p in ["/var/memory/a.md", "/var/memory/b.md", "/var/memory/c.md"] {
        let path = MemoryPath::parse(p).unwrap();
        let chunk = IndexedChunk {
            chunk_index: 0,
            locator: Locator::Text { start: 0, end: 4 },
            text: "xxxx".into(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
        };
        idx.upsert(&tenant, &path, MemoryVersion(1), &embedder_id, std::slice::from_ref(&chunk)).unwrap();
    }
    idx.enqueue_backlog_from_chunks(&tenant).unwrap();
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 3);

    // Batch pulls oldest rows; we asked for 2.
    let batch = idx.take_backlog_batch(&tenant, 2).unwrap();
    assert_eq!(batch.len(), 2);

    // Delete one.
    idx.delete_backlog_row(&tenant, &batch[0].path, batch[0].version).unwrap();
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 2);

    // Bump retry on the other; it stays.
    idx.bump_backlog_retry(&tenant, &batch[1].path, batch[1].version, "embedder err").unwrap();
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 2);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index take_backlog_batch 2>&1 | tail -10
```

Expected: compile error.

- [ ] **Step 3: Add `BacklogRow` struct and trait methods**

In `crates/simulacra-memory/src/index.rs`, add at the top (near `IndexedChunk`):

```rust
/// A row pulled from `memory_embed_backlog` by the backlog-draining
/// worker. Identifies a `(path, version)` that needs embeddings
/// regenerated.
#[derive(Debug, Clone)]
pub struct BacklogRow {
    pub path: MemoryPath,
    pub version: MemoryVersion,
    pub retry_count: u32,
}
```

And in the `VectorIndex` trait:

```rust
    /// S037 §13: take up to `batch_size` backlog rows, oldest first, for
    /// processing by the background worker. Rows remain in the table —
    /// the worker deletes them on success via `delete_backlog_row` or
    /// bumps retry via `bump_backlog_retry` on failure. Ordering is
    /// `(retry_count ASC, enqueued_at ASC)` so failed rows do not
    /// starve fresh ones.
    fn take_backlog_batch(
        &self,
        _tenant: &TenantId,
        _batch_size: usize,
    ) -> Result<Vec<BacklogRow>, MemoryError> {
        Ok(Vec::new())
    }

    fn delete_backlog_row(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
    ) -> Result<(), MemoryError> {
        Ok(())
    }

    fn bump_backlog_retry(
        &self,
        _tenant: &TenantId,
        _path: &MemoryPath,
        _version: MemoryVersion,
        _last_error: &str,
    ) -> Result<(), MemoryError> {
        Ok(())
    }
```

- [ ] **Step 4: Implement on `SqliteVectorIndex`**

In `crates/simulacra-memory/src/sqlite_index.rs`:

```rust
    fn take_backlog_batch(
        &self,
        tenant: &TenantId,
        batch_size: usize,
    ) -> Result<Vec<BacklogRow>, MemoryError> {
        let lock = self.tenant_lock(tenant);
        let _guard = lock.lock().expect("per-tenant write lock poisoned");
        let conn = self.open_conn(tenant)?;
        let mut stmt = conn.prepare(
            "SELECT path, version, retry_count FROM memory_embed_backlog
             ORDER BY retry_count ASC, enqueued_at ASC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([batch_size as i64], |row| {
                let path_str: String = row.get(0)?;
                let version: i64 = row.get(1)?;
                let retry_count: i64 = row.get(2)?;
                Ok((path_str, version, retry_count))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (path_str, version, retry_count) in rows {
            let path = MemoryPath::parse(&path_str)
                .map_err(|e| MemoryError::Internal(format!("backlog path: {e}")))?;
            out.push(BacklogRow {
                path,
                version: MemoryVersion(version as u64),
                retry_count: retry_count as u32,
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
```

- [ ] **Step 5: Run test to verify it passes**

```bash
cargo test -p simulacra-memory --test sqlite_vector_index take_backlog_batch 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-memory/src/index.rs crates/simulacra-memory/src/sqlite_index.rs crates/simulacra-memory/tests/sqlite_vector_index.rs
git commit -m "feat(simulacra-memory): add backlog worker primitives [S037]"
```

---

## Task 7: Backlog-draining worker in `BackgroundEmbedder`

**Files:**
- Modify: `crates/simulacra-memory/src/background.rs` (new worker loop)
- Modify: `crates/simulacra-memory/tests/background_embedder.rs` (tests)

- [ ] **Step 1: Write the failing tests**

Append to `crates/simulacra-memory/tests/background_embedder.rs`:

```rust
// S037 1140: backlog worker drains same-dim reindex backlog rows and
// writes new vectors keyed to the new embedder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlog_worker_drains_reindex_backlog_and_writes_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();

    // Seed with EmbedderA.
    let store = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
    let old_id = EmbedderId::new("fake@v1:4").unwrap();
    let idx = Arc::new(SqliteVectorIndex::new(dir.path(), old_id.clone(), 4).unwrap());
    let path = MemoryPath::parse("/var/memory/a.md").unwrap();
    store.put(&tenant, &path, MemoryVersion(1), b"aaaa").unwrap();
    let chunk = IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text { start: 0, end: 4 },
        text: "aaaa".into(),
        embedding: vec![1.0, 0.0, 0.0, 0.0],
    };
    idx.upsert(&tenant, &path, MemoryVersion(1), &old_id, std::slice::from_ref(&chunk)).unwrap();

    // Simulate same-dim reindex staging: clear vectors, enqueue backlog, set new id.
    idx.mark_tenant_stale(&tenant).unwrap();
    idx.enqueue_backlog_from_chunks(&tenant).unwrap();
    SqliteVectorIndex::set_embedder_id_at(dir.path(), &tenant, &EmbedderId::new("fake@v2:4").unwrap()).unwrap();

    // Spawn background embedder with the new embedder.
    let new_id = EmbedderId::new("fake@v2:4").unwrap();
    let idx2 = Arc::new(SqliteVectorIndex::new(dir.path(), new_id.clone(), 4).unwrap());
    let embedder = Arc::new(FakeEmbedder::new(new_id.clone(), 4)) as Arc<dyn Embedder>;
    let be = BackgroundEmbedder::spawn(
        store.clone(),
        idx2.clone(),
        embedder,
        Arc::new(|_: &MemoryPath| Some(Arc::new(FixedTokenChunker::default()) as Arc<dyn Chunker>)),
        BackgroundEmbedderConfig::default(),
    ).unwrap();

    // Wait for the worker to drain the backlog (bounded).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while idx2.backlog_count(&tenant).unwrap() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(idx2.backlog_count(&tenant).unwrap(), 0, "backlog drained");

    be.shutdown().await.unwrap();
}

// S037 1143: wipe_and_rebuild path — chunks are absent, worker reads
// content, re-chunks, then embeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlog_worker_rechunks_from_content_when_chunks_absent() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let new_id = EmbedderId::new("fake@v2:8").unwrap();

    // Seed content only; no chunks (simulates post-wipe state).
    let store = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
    let path = MemoryPath::parse("/var/memory/a.md").unwrap();
    store.put(&tenant, &path, MemoryVersion(1), b"hello world").unwrap();

    let idx = Arc::new(SqliteVectorIndex::new(dir.path(), new_id.clone(), 8).unwrap());
    idx.enqueue_backlog_from_content(&tenant).unwrap();

    let embedder = Arc::new(FakeEmbedder::new(new_id.clone(), 8)) as Arc<dyn Embedder>;
    let be = BackgroundEmbedder::spawn(
        store.clone(),
        idx.clone(),
        embedder,
        Arc::new(|_: &MemoryPath| Some(Arc::new(FixedTokenChunker::default()) as Arc<dyn Chunker>)),
        BackgroundEmbedderConfig::default(),
    ).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while idx.backlog_count(&tenant).unwrap() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 0);
    // Verify chunks now exist.
    let chunk = idx.get_chunk(&tenant, &path, MemoryVersion(1), 0).unwrap();
    assert!(chunk.is_some(), "chunk re-created from content");

    be.shutdown().await.unwrap();
}

// S037: failure path — embedder errors bump retry_count and leave the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlog_worker_bumps_retry_count_on_embedder_failure() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let new_id = EmbedderId::new("fake@v2:4").unwrap();

    let store = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
    let idx = Arc::new(SqliteVectorIndex::new(dir.path(), new_id.clone(), 4).unwrap());
    let path = MemoryPath::parse("/var/memory/a.md").unwrap();
    store.put(&tenant, &path, MemoryVersion(1), b"xx").unwrap();
    idx.enqueue_backlog_from_content(&tenant).unwrap();
    assert_eq!(idx.backlog_count(&tenant).unwrap(), 1);

    let embedder = Arc::new(FailingEmbedder::new(new_id.clone(), 4)) as Arc<dyn Embedder>;
    let be = BackgroundEmbedder::spawn(
        store.clone(),
        idx.clone(),
        embedder,
        Arc::new(|_: &MemoryPath| Some(Arc::new(FixedTokenChunker::default()) as Arc<dyn Chunker>)),
        BackgroundEmbedderConfig::default(),
    ).unwrap();

    // Give the worker a window to attempt the row at least twice.
    tokio::time::sleep(Duration::from_millis(500)).await;
    be.shutdown().await.unwrap();

    // Row still there with retry_count >= 1.
    let batch = idx.take_backlog_batch(&tenant, 10).unwrap();
    assert_eq!(batch.len(), 1);
    assert!(batch[0].retry_count >= 1, "retry_count bumped");
}
```

Check the file's existing imports at the top — may need to add `FailingEmbedder` (a new fake that always returns `Err`). If one doesn't exist, add it to whichever test helpers module holds `FakeEmbedder`.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p simulacra-memory --test background_embedder backlog_worker 2>&1 | tail -20
```

Expected: compile error or runtime failure — no backlog worker exists yet.

- [ ] **Step 3: Implement the backlog worker**

In `crates/simulacra-memory/src/background.rs`:

1. Add a constant for the idle poll interval near `DEFAULT_QUEUE_CAPACITY`:

```rust
/// How long the backlog-draining worker sleeps when the backlog is empty
/// before polling again. Kept short so startup reindex progresses
/// quickly; long enough to not burn CPU.
pub const BACKLOG_DRAIN_IDLE_MS: u64 = 250;
/// Maximum retry_count before the backlog row is treated as
/// dead-lettered (row remains, worker stops attempting it).
pub const BACKLOG_MAX_RETRIES: u32 = 10;
/// Batch size for backlog pulls; keeps per-tx work bounded.
pub const BACKLOG_BATCH_SIZE: usize = 32;
```

2. Add a second worker function alongside the existing per-tenant Put-event worker. Place it near the existing worker function (search `fn tenant_worker_loop` or similar):

```rust
async fn tenant_backlog_drain_loop(
    tenant: TenantId,
    store: Arc<dyn MemoryStore>,
    index: Arc<dyn VectorIndex>,
    embedder: Arc<dyn Embedder>,
    chunker_selector: ChunkerSelector,
    config: Arc<BackgroundEmbedderConfig>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let idle = Duration::from_millis(BACKLOG_DRAIN_IDLE_MS);
    loop {
        if *shutdown.borrow() {
            return;
        }
        let rows = match index.take_backlog_batch(&tenant, BACKLOG_BATCH_SIZE) {
            Ok(r) => r,
            Err(e) => {
                warn!(tenant = %tenant, error = %e, "backlog read failed");
                tokio::time::sleep(idle).await;
                continue;
            }
        };
        if rows.is_empty() {
            tokio::select! {
                _ = tokio::time::sleep(idle) => continue,
                _ = shutdown.changed() => return,
            }
        }
        for row in rows {
            if row.retry_count >= BACKLOG_MAX_RETRIES {
                // Dead-lettered — skip but leave row in place so the metric reflects it.
                continue;
            }
            match process_backlog_row(
                &tenant,
                &row,
                store.as_ref(),
                index.as_ref(),
                embedder.as_ref(),
                &chunker_selector,
            ).await {
                Ok(()) => {
                    if let Err(e) = index.delete_backlog_row(&tenant, &row.path, row.version) {
                        warn!(tenant = %tenant, path = %row.path, error = %e, "delete backlog row failed");
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if let Err(bumpe) = index.bump_backlog_retry(&tenant, &row.path, row.version, &msg) {
                        warn!(tenant = %tenant, path = %row.path, error = %bumpe, "bump retry failed");
                    }
                }
            }
        }
    }
}

async fn process_backlog_row(
    tenant: &TenantId,
    row: &BacklogRow,
    store: &dyn MemoryStore,
    index: &dyn VectorIndex,
    embedder: &dyn Embedder,
    chunker_selector: &ChunkerSelector,
) -> Result<(), MemoryError> {
    // Load existing chunks for (path, version); if empty, re-chunk from content.
    let mut existing = index.load_chunks_for(tenant, &row.path, row.version)?;  // see note
    if existing.is_empty() {
        let content = match store.get(tenant, &row.path)? {
            Some(c) if c.version == row.version => c,
            _ => return Ok(()),  // content changed or deleted; drop the row
        };
        let chunker = chunker_selector(&row.path)
            .ok_or_else(|| MemoryError::Internal(format!("no chunker for {}", row.path)))?;
        let chunks = chunker.chunk(&content.data)?;
        existing = chunks.into_iter().enumerate()
            .map(|(i, c)| IndexedChunk {
                chunk_index: i,
                locator: c.locator,
                text: c.text,
                embedding: Vec::new(),  // to be filled
            })
            .collect();
        // Persist chunks without embeddings via upsert_chunks_only.
        index.upsert_chunks_only(tenant, &row.path, row.version, &existing)?;
    }
    let texts: Vec<&str> = existing.iter().map(|c| c.text.as_str()).collect();
    let vectors = embedder.embed_batch(&texts).await?;
    let embedder_id = embedder.id();
    let indexed: Vec<IndexedChunk> = existing.into_iter().zip(vectors.into_iter())
        .map(|(mut c, v)| { c.embedding = v; c })
        .collect();
    index.upsert(tenant, &row.path, row.version, &embedder_id, &indexed)?;
    Ok(())
}
```

Note on `load_chunks_for` / `upsert_chunks_only`: these are new helper methods on `VectorIndex` that this task adds. Signatures:

```rust
/// Return all IndexedChunk rows for `(tenant, path, version)`. Used by
/// the backlog worker to decide whether to re-embed existing chunks
/// (`reindex_background` path) or re-chunk from content
/// (`wipe_and_rebuild` path). Returned chunks may have empty
/// `embedding` fields if they were written by `upsert_chunks_only`.
fn load_chunks_for(
    &self,
    tenant: &TenantId,
    path: &MemoryPath,
    version: MemoryVersion,
) -> Result<Vec<IndexedChunk>, MemoryError>;

/// Write chunks to `memory_chunks` without any corresponding vectors
/// in `memory_vectors`. Used by the backlog worker in the
/// `wipe_and_rebuild` path: re-chunk content, persist text rows,
/// then embed + upsert vectors in a follow-up call. Chunks written
/// this way are surfaced by `load_chunks_for` with empty embeddings.
fn upsert_chunks_only(
    &self,
    tenant: &TenantId,
    path: &MemoryPath,
    version: MemoryVersion,
    chunks: &[IndexedChunk],
) -> Result<(), MemoryError>;
```

Add both methods to the `VectorIndex` trait in `crates/simulacra-memory/src/index.rs` alongside the others, implement on `SqliteVectorIndex`, and add a quick unit test pair in `sqlite_vector_index.rs` (seed via `upsert_chunks_only` → read back via `load_chunks_for` → assert row count and empty embeddings).

3. In `BackgroundEmbedder::spawn`, after the existing per-tenant Put worker is spawned, also spawn the backlog drain worker. Thread a `tokio::sync::watch::Sender<bool>` from `spawn` through to `shutdown` so both workers see the same shutdown signal.

4. At the top of the dispatcher, also seed a backlog drain worker for every `known_tenants()` entry — so tenants with a pre-existing backlog but no active Put traffic still get their backlog drained.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p simulacra-memory --test background_embedder backlog_worker 2>&1 | tail -10
```

Expected: `3 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-memory/src/background.rs crates/simulacra-memory/src/index.rs crates/simulacra-memory/src/sqlite_index.rs crates/simulacra-memory/tests/background_embedder.rs
git commit -m "feat(simulacra-memory): add backlog-draining worker [S037]"
```

---

## Task 8: `SemanticSearchTool` records `memory.search.reindexing` span attr

**Files:**
- Modify: `crates/simulacra-tool/src/memory.rs`
- Modify: `crates/simulacra-server/tests/memory_hook_integration.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/simulacra-server/tests/memory_hook_integration.rs`:

```rust
// S037 1144: semantic_search records memory.search.reindexing=true when
// backlog is non-empty for the tenant.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_records_reindexing_true_when_backlog_nonempty() {
    let fixture = MemoryToolFixture::new().await;
    // Seed a backlog row directly (bypass worker).
    fixture.index.enqueue_backlog_from_content(&fixture.tenant).unwrap();
    assert!(fixture.index.backlog_count(&fixture.tenant).unwrap() > 0);

    let (span_values, _) = capture_with_subscriber(|| async {
        fixture.invoke_semantic_search("anything").await
    }).await;

    let search_span = span_values.iter().find(|s| s.name == "memory_search")
        .expect("memory_search span emitted");
    assert_eq!(
        search_span.fields.get("memory.search.reindexing").as_deref(),
        Some("true"),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_records_reindexing_false_when_backlog_empty() {
    let fixture = MemoryToolFixture::new().await;
    assert_eq!(fixture.index.backlog_count(&fixture.tenant).unwrap(), 0);

    let (span_values, _) = capture_with_subscriber(|| async {
        fixture.invoke_semantic_search("anything").await
    }).await;

    let search_span = span_values.iter().find(|s| s.name == "memory_search")
        .expect("memory_search span emitted");
    assert_eq!(
        search_span.fields.get("memory.search.reindexing").as_deref(),
        Some("false"),
    );
}
```

The fixture API here is a placeholder — the existing `memory_hook_integration.rs` already uses a similar harness (`CaptureLayer` / `capture_with_subscriber`). Match whatever pattern is there; the assertion shape is what matters.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p simulacra-server --test memory_hook_integration reindexing 2>&1 | tail -10
```

Expected: FAIL — field `memory.search.reindexing` is not recorded.

- [ ] **Step 3: Add the field to the span and record it**

In `crates/simulacra-tool/src/memory.rs`, find the `memory_search` span declaration (around line 171 in `SemanticSearchTool::invoke`). Add `memory.search.reindexing = tracing::field::Empty` alongside the other empty fields, then record the value:

```rust
// Before the tool search runs:
let reindexing = self.index.backlog_count(&tenant).unwrap_or(0) > 0;
span.record("memory.search.reindexing", reindexing);
```

Place the `span.record` call immediately after `span` is entered and before any search work, so the attr is set even if search fails.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p simulacra-server --test memory_hook_integration reindexing 2>&1 | tail -5
```

Expected: `2 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-tool/src/memory.rs crates/simulacra-server/tests/memory_hook_integration.rs
git commit -m "feat(simulacra-tool): record memory.search.reindexing span attr [S037]"
```

---

## Task 9: Startup policy dispatch in `simulacra-cli`

**Files:**
- Modify: `crates/simulacra-cli/src/lib.rs`
- Create: `crates/simulacra-memory/src/reindex_startup.rs`
- Modify: `crates/simulacra-memory/src/sqlite_index.rs` (add `_at` associated-function variants)
- Create: `crates/simulacra-memory/tests/model_change_reindex.rs`

**Prerequisite — `_at` helpers.** The startup dispatch has no live `&SqliteVectorIndex` (the constructor would refuse a mismatched fingerprint). The policy helpers need associated-function variants that open a raw `Connection` (sqlite-vec init + no validation) and apply the same SQL as their trait-method counterparts. Add these to `impl SqliteVectorIndex` alongside `wipe_and_reopen`:

| Associated fn | Mirrors trait method |
|---|---|
| `SqliteVectorIndex::read_fingerprint(root, tenant) -> Option<(EmbedderId, usize)>` | `embedder_fingerprint` |
| `SqliteVectorIndex::mark_tenant_stale_at(root, tenant) -> u64` | `mark_tenant_stale` |
| `SqliteVectorIndex::enqueue_backlog_from_chunks_at(root, tenant) -> u64` | `enqueue_backlog_from_chunks` |
| `SqliteVectorIndex::enqueue_backlog_from_content_at(root, tenant) -> u64` | `enqueue_backlog_from_content` |
| `SqliteVectorIndex::set_embedder_id_at(root, tenant, new_id) -> ()` | (adds new behaviour — no `&self` variant) |
| `SqliteVectorIndex::record_embedder_log_at(root, tenant, id, action) -> ()` | existing `record_embedder_log` if it exists; else new |

Each is a copy of the trait-method body with `self.open_conn(tenant)` replaced by the same raw `Connection::open + sqlite-vec init` pattern used in `wipe_and_reopen`. No new tests required — the `model_change_reindex` integration tests exercise them end-to-end.

- [ ] **Step 1: Write the failing integration tests**

Create `crates/simulacra-memory/tests/model_change_reindex.rs`:

```rust
// S037 1140: same-dim reindex_background end-to-end.
//
// Seeds a tenant with EmbedderA, closes, reopens with EmbedderB (same
// dim, different name/version) using on_model_change = reindex_background,
// asserts:
//   - vectors are cleared at startup (mark_tenant_stale applied)
//   - backlog is populated with (path, version) rows from memory_chunks
//   - memory_schema_meta now carries the new EmbedderB id
//   - after the background worker runs, backlog empty and vectors exist
//     under EmbedderB.

use simulacra_memory::{/* … */};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_dim_reindex_background_re_embeds_existing_chunks() {
    // Step 1: first session with EmbedderA — seed and shut down.
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let old_id = EmbedderId::new("fake@v1:4").unwrap();
    {
        let store = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
        let index = Arc::new(SqliteVectorIndex::new(dir.path(), old_id.clone(), 4).unwrap());
        store.put(&tenant, &MemoryPath::parse("/var/memory/a.md").unwrap(),
                  MemoryVersion(1), b"aaaa").unwrap();
        let chunk = IndexedChunk {
            chunk_index: 0,
            locator: Locator::Text { start: 0, end: 4 },
            text: "aaaa".into(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
        };
        index.upsert(&tenant, &MemoryPath::parse("/var/memory/a.md").unwrap(),
                     MemoryVersion(1), &old_id, std::slice::from_ref(&chunk)).unwrap();
    }

    // Step 2: apply reindex_background policy from simulacra-cli's helper.
    let new_id = EmbedderId::new("fake@v2:4").unwrap();
    simulacra_cli::apply_on_model_change_policy(
        dir.path(),
        &tenant,
        &new_id,
        simulacra_config::OnModelChange::ReindexBackground,
    ).expect("policy applied");

    // Step 3: open index with EmbedderB, spawn background embedder.
    let store = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
    let index = Arc::new(SqliteVectorIndex::new(dir.path(), new_id.clone(), 4).unwrap());
    let embedder = Arc::new(FakeEmbedder::new(new_id.clone(), 4)) as Arc<dyn Embedder>;
    let be = BackgroundEmbedder::spawn(
        store.clone(),
        index.clone(),
        embedder,
        Arc::new(|_: &MemoryPath| Some(Arc::new(FixedTokenChunker::default()) as Arc<dyn Chunker>)),
        BackgroundEmbedderConfig::default(),
    ).unwrap();

    // Step 4: wait for drain.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while index.backlog_count(&tenant).unwrap() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    // Step 5: vectors exist under new embedder id.
    let hit = index.search(
        &tenant,
        &MemoryPath::parse("/var/memory").unwrap(),
        &[1.0, 0.0, 0.0, 0.0],
        &new_id,
        1,
        None,
    ).unwrap();
    assert_eq!(hit.len(), 1);

    be.shutdown().await.unwrap();
}

// S037 1143: different-dim wipe_and_rebuild end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_dim_wipe_and_rebuild_rebuilds_from_content() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new("cli").unwrap();
    let old_id = EmbedderId::new("fake@v1:4").unwrap();
    {
        let store = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
        let index = Arc::new(SqliteVectorIndex::new(dir.path(), old_id.clone(), 4).unwrap());
        store.put(&tenant, &MemoryPath::parse("/var/memory/a.md").unwrap(),
                  MemoryVersion(1), b"hello world").unwrap();
        let chunk = IndexedChunk {
            chunk_index: 0,
            locator: Locator::Text { start: 0, end: 11 },
            text: "hello world".into(),
            embedding: vec![1.0, 0.0, 0.0, 0.0],
        };
        index.upsert(&tenant, &MemoryPath::parse("/var/memory/a.md").unwrap(),
                     MemoryVersion(1), &old_id, std::slice::from_ref(&chunk)).unwrap();
    }

    let new_id = EmbedderId::new("fake@v2:8").unwrap();
    simulacra_cli::apply_on_model_change_policy(
        dir.path(),
        &tenant,
        &new_id,
        simulacra_config::OnModelChange::WipeAndRebuild,
    ).expect("policy applied");

    let store = Arc::new(SqliteMemoryStore::new(dir.path()).unwrap());
    let index = Arc::new(SqliteVectorIndex::new(dir.path(), new_id.clone(), 8).unwrap());
    let embedder = Arc::new(FakeEmbedder::new(new_id.clone(), 8)) as Arc<dyn Embedder>;
    let be = BackgroundEmbedder::spawn(
        store.clone(),
        index.clone(),
        embedder,
        Arc::new(|_: &MemoryPath| Some(Arc::new(FixedTokenChunker::default()) as Arc<dyn Chunker>)),
        BackgroundEmbedderConfig::default(),
    ).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while index.backlog_count(&tenant).unwrap() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    // New dim vectors exist and search succeeds.
    let query = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let hit = index.search(
        &tenant,
        &MemoryPath::parse("/var/memory").unwrap(),
        &query,
        &new_id,
        1,
        None,
    ).unwrap();
    assert_eq!(hit.len(), 1);

    be.shutdown().await.unwrap();
}
```

Notes:
- `simulacra-memory/tests/model_change_reindex.rs` needs a `dev-dependencies` entry for `simulacra-cli` in `crates/simulacra-memory/Cargo.toml` — or alternatively, the helper lives in a lower crate. **Prefer: put `apply_on_model_change_policy` in a new `simulacra-memory::reindex_startup` module** and have `simulacra-cli` call it. That avoids the circular dev-dep.
- Rewrite the test calls to `simulacra_memory::reindex_startup::apply_policy(...)` if that's where the helper ends up.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p simulacra-memory --test model_change_reindex 2>&1 | tail -10
```

Expected: compile error — `apply_policy` doesn't exist.

- [ ] **Step 3: Implement `reindex_startup` module in `simulacra-memory`**

Create `crates/simulacra-memory/src/reindex_startup.rs`:

```rust
//! S037 §13: startup policy dispatch for embedder mismatch.
//!
//! Called by the CLI bootstrap after reading `MemoryConfig.on_model_change`.
//! Inspects the stored embedder fingerprint, compares against the
//! configured one, and applies the policy. Returns Ok(()) when the
//! tenant is ready for the caller to construct a reconciled index.

use std::path::Path;

use simulacra_types::TenantId;

use crate::embedder::EmbedderId;
use crate::error::MemoryError;
use crate::sqlite_index::SqliteVectorIndex;

/// Policy variant shared with simulacra-config. We re-declare here (rather
/// than depending on simulacra-config) to keep crate layering clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnModelChangePolicy {
    Refuse,
    ReindexBackground,
    WipeAndRebuild,
}

/// S037 §13: apply the configured policy when a tenant's stored
/// embedder does not match the configured one. Called from simulacra-cli
/// bootstrap after reading stored fingerprint.
///
/// - `refuse` returns `EmbedderMismatch` / `EmbedderDimensionMismatch`
/// - `reindex_background` (same dim): clear vectors, enqueue chunks,
///   update meta. Different dim: `EmbedderDimensionMismatch`.
/// - `wipe_and_rebuild` (any dim): drop+recreate vectors+chunks,
///   enqueue content, update meta.
pub fn apply_policy(
    root: &Path,
    tenant: &TenantId,
    configured: &EmbedderId,
    configured_dim: usize,
    policy: OnModelChangePolicy,
) -> Result<(), MemoryError> {
    // Read stored fingerprint without going through the full constructor
    // (which would error on mismatch).
    let stored = SqliteVectorIndex::read_fingerprint(root, tenant)?;
    let Some((stored_id, stored_dim)) = stored else {
        return Ok(());  // fresh tenant, nothing to do
    };

    let same_dim = stored_dim == configured_dim;
    let same_id = stored_id.as_str() == configured.as_str();
    if same_id && same_dim {
        return Ok(());  // no mismatch
    }

    match policy {
        OnModelChangePolicy::Refuse => {
            if !same_dim {
                Err(MemoryError::EmbedderDimensionMismatch {
                    stored: stored_dim,
                    configured: configured_dim,
                })
            } else {
                Err(MemoryError::EmbedderMismatch {
                    stored: stored_id.as_str().to_string(),
                    configured: configured.as_str().to_string(),
                    requires_wipe: false,
                })
            }
        }
        OnModelChangePolicy::ReindexBackground => {
            if !same_dim {
                return Err(MemoryError::EmbedderDimensionMismatch {
                    stored: stored_dim,
                    configured: configured_dim,
                });
            }
            SqliteVectorIndex::mark_tenant_stale_at(root, tenant)?;
            SqliteVectorIndex::enqueue_backlog_from_chunks_at(root, tenant)?;
            SqliteVectorIndex::set_embedder_id_at(root, tenant, configured)?;
            SqliteVectorIndex::record_embedder_log_at(root, tenant, configured, "reindex")?;
            Ok(())
        }
        OnModelChangePolicy::WipeAndRebuild => {
            let cleared = SqliteVectorIndex::wipe_and_reopen(
                root, tenant, configured.clone(), configured_dim,
            )?;
            SqliteVectorIndex::enqueue_backlog_from_content_at(root, tenant)?;
            SqliteVectorIndex::record_embedder_log_at(root, tenant, configured, "wipe_and_rebuild")?;
            let _ = cleared;
            Ok(())
        }
    }
}
```

Add `pub mod reindex_startup;` at the top of `crates/simulacra-memory/src/lib.rs`, plus the `pub use reindex_startup::{apply_policy, OnModelChangePolicy};` re-export.

The `_at` associated-function variants (`mark_tenant_stale_at`, `enqueue_backlog_from_chunks_at`, `enqueue_backlog_from_content_at`, `set_embedder_id_at`, `record_embedder_log_at`, `read_fingerprint`) are new `impl SqliteVectorIndex` functions that open their own `Connection` (they cannot rely on `&self` because the constructor refuses mismatches). Copy the body of the trait-method equivalents and swap `self.open_conn(tenant)` for a direct `Connection::open` with sqlite-vec init.

- [ ] **Step 4: Wire into `simulacra-cli` bootstrap**

In `crates/simulacra-cli/src/lib.rs`, around line 576 (the `DefaultEmbedder::load_default` call), add before the index is constructed:

```rust
// Map simulacra_config::OnModelChange to simulacra_memory::OnModelChangePolicy.
let policy = match memory_config.on_model_change {
    simulacra_config::OnModelChange::Refuse => simulacra_memory::OnModelChangePolicy::Refuse,
    simulacra_config::OnModelChange::ReindexBackground => simulacra_memory::OnModelChangePolicy::ReindexBackground,
    simulacra_config::OnModelChange::WipeAndRebuild => simulacra_memory::OnModelChangePolicy::WipeAndRebuild,
};
let embedder_id = embedder_concrete.id();
simulacra_memory::apply_policy(
    &memory_config.dir,
    &tenant,
    &embedder_id,
    embedder_concrete.dim(),
    policy,
)?;
// now the SqliteVectorIndex constructor will not see a mismatch.
```

Adjust naming (`tenant`, `embedder_concrete.dim()`) to whatever the surrounding code uses.

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p simulacra-memory --test model_change_reindex 2>&1 | tail -5
```

Expected: `2 passed`.

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-memory/src/reindex_startup.rs crates/simulacra-memory/src/sqlite_index.rs crates/simulacra-memory/src/lib.rs crates/simulacra-cli/src/lib.rs crates/simulacra-memory/tests/model_change_reindex.rs
git commit -m "feat(simulacra-memory): startup dispatch for on_model_change policy [S037]"
```

---

## Task 10: Full-suite mechanical gate + observability

- [ ] **Step 1: Run the full mechanical gate**

```bash
cargo build --workspace 2>&1 | tail -20
cargo test --workspace 2>&1 | tail -20
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20
cargo fmt --all -- --check 2>&1 | tail -5
```

All four must pass. Fix any regressions surfaced in other crates.

- [ ] **Step 2: Obsidian metric check (observability validation per `rules/R010`)**

Run the wipe_and_rebuild integration test with OTLP export to Obsidian, then verify the `simulacra_memory_reindex_backlog` gauge ticks down to zero as the worker drains.

```bash
# Example — adjust flags to match how local dev runs export to Obsidian.
SIMULACRA_OTLP_ENDPOINT=http://localhost:4317 \
    cargo test -p simulacra-memory --test model_change_reindex -- --nocapture
```

Then query Obsidian PromQL:
```
simulacra_memory_reindex_backlog{tenant="cli"}
```
Expected: non-zero spike during the test run, drops to 0 as the worker drains.

- [ ] **Step 3: Commit any fix-ups**

```bash
git status
git add -p   # stage only relevant hunks
git commit -m "fix(simulacra-memory): clippy/fmt cleanup around reindex integration [S037]"
```

---

## Task 11: Spec flips

**Files:**
- Modify: `specs/S037-memory-and-semantic-retrieval.md`

- [ ] **Step 1: Flip lines 1140, 1143, 1144**

In `specs/S037-memory-and-semantic-retrieval.md`, change each `- [ ]` to `- [x]` and update the trailing annotation:

- Line 1140:
  ```markdown
  - [x] Same dim, different name/version, `on_model_change = reindex_background`: `mark_tenant_stale` called, background embedder re-embeds (impl: `simulacra_memory::reindex_startup::apply_policy`, test: `crates/simulacra-memory/tests/model_change_reindex.rs::same_dim_reindex_background_re_embeds_existing_chunks`)
  ```
- Line 1143:
  ```markdown
  - [x] Different dim, `on_model_change = wipe_and_rebuild`: `memory_vectors` and `memory_chunks` are dropped and recreated with new dim; content is re-chunked and re-embedded from `memory_content` (impl: `SqliteVectorIndex::wipe_and_reopen` + backlog worker, test: `crates/simulacra-memory/tests/model_change_reindex.rs::different_dim_wipe_and_rebuild_rebuilds_from_content`)
  ```
- Line 1144:
  ```markdown
  - [x] During reindex, `semantic_search` still works but returns `memory.search.reindexing=true` span attribute (impl: `SemanticSearchTool::invoke` records from `VectorIndex::backlog_count`, test: `crates/simulacra-server/tests/memory_hook_integration.rs::semantic_search_records_reindexing_true_when_backlog_nonempty`)
  ```

- [ ] **Step 2: Bump the header counter**

Locate the "Acceptance criteria" header counter in S037 (it shows `N/149` somewhere near the top of the assertion list) and bump by 3.

- [ ] **Step 3: Commit**

```bash
git add specs/S037-memory-and-semantic-retrieval.md
git commit -m "docs(specs): check off S037 1140/1143/1144 reindex integration [S037]"
```

---

## Verification (self-review checklist for the engineer)

- [ ] `cargo build --workspace` — clean
- [ ] `cargo test --workspace` — all green
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — clean
- [ ] `cargo fmt --all -- --check` — clean
- [ ] `simulacra_memory_reindex_backlog` gauge visible in Obsidian during the integration test run
- [ ] `specs/S037-memory-and-semantic-retrieval.md` updated with impl/test pointers on flipped assertions
- [ ] No uncommitted diff in `crates/simulacra-provider/src/openai/mod.rs` (session-initial modification — leave alone unless separately addressed)
