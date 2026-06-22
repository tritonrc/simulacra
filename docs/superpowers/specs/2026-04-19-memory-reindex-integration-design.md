# Memory Reindex Integration — Design

**Date:** 2026-04-19
**Owner:** Brian McKinney
**Spec reference:** `specs/S037-memory-and-semantic-retrieval.md` — assertions 1140, 1143, 1144

## Problem

`VectorIndex::mark_tenant_stale` exists and the startup path detects `EmbedderMismatch`, but nothing connects policy → bulk re-embed → reindexing flag. Three S037 assertions remain:

- **1140** — Same dim, different name/version, `on_model_change = reindex_background`: `mark_tenant_stale` called, background embedder re-embeds.
- **1143** — Different dim, `on_model_change = wipe_and_rebuild`: `memory_vectors` and `memory_chunks` dropped and recreated with new dim; content re-chunked and re-embedded from `memory_content`.
- **1144** — During reindex, `semantic_search` still works but records `memory.search.reindexing=true` span attribute.

Today:
- `MemoryConfig` in `simulacra-config` has no `on_model_change` field — config knob doesn't exist.
- Startup propagates `EmbedderMismatch` as an error; no policy dispatch.
- `BackgroundEmbedder` reacts only to `MemoryEvent::Put` — no backfill scan, no backlog drainer.
- `SemanticSearchTool` records no reindexing attr.

This design ships all three assertions as one deliverable.

## Design decisions

1. **Scope** — ship all three assertions together. The config surface (`on_model_change`) is identical across policies and both `reindex_background` and `wipe_and_rebuild` need the same backlog-draining worker + reindexing flag.
2. **Discovery of stale chunks** — backlog-driven. `mark_tenant_stale` (and its wipe counterpart) populate `memory_embed_backlog` with one row per (path, version). A new backlog-draining worker pulls rows, embeds, upserts, deletes. The existing `memory_embed_backlog` table and `simulacra_memory_reindex_backlog` gauge already exist for exactly this purpose.
3. **Reindexing flag** — derived from `VectorIndex::backlog_count(tenant) > 0`. Single source of truth (the backlog table), automatic crash recovery, per-search cost is a tenant-scoped `SELECT COUNT(*)` on an indexed table.
4. **wipe_and_rebuild startup behavior** — startup is O(paths): drop+recreate vectors with new dim, drop+recreate chunks, enqueue `memory_content` paths into backlog. Worker re-chunks + re-embeds in the background.
5. **Unified worker behavior** — one backlog-draining loop handles both policies. For each `(path, version)` row: if `memory_chunks` has rows for the key, re-embed those; otherwise load from `memory_content`, chunk, upsert chunks, then embed.

## Architecture

### Config (`simulacra-config`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnModelChange {
    #[default]
    Refuse,
    ReindexBackground,
    WipeAndRebuild,
}

pub struct MemoryConfig {
    pub dir: std::path::PathBuf,
    pub tenant: String,
    pub retention: Option<MemoryRetentionConfig>,
    #[serde(default)]
    pub on_model_change: OnModelChange,
}
```

TOML example:
```toml
[memory]
dir = "./.simulacra-memory"
tenant = "cli"
on_model_change = "reindex_background"
```

No cross-field validation — the runtime enforces "different dim + `reindex_background`" → `EmbedderDimensionMismatch` (spec §13).

### Startup policy dispatch (`simulacra-cli`)

```rust
match index.ensure_tenant_with_embedder(&tenant, &embedder_id) {
    Ok(()) => { /* normal path */ }
    Err(MemoryError::EmbedderMismatch { stored, configured, requires_wipe }) => {
        match memory_config.on_model_change {
            OnModelChange::Refuse => return Err(/* surface mismatch */),

            OnModelChange::ReindexBackground if !requires_wipe => {
                let cleared = index.mark_tenant_stale(&tenant)?;
                index.enqueue_backlog_from_chunks(&tenant)?;
                index.record_embedder_log(&tenant, &embedder_id, cleared, "reindex")?;
                index.set_embedder_id(&tenant, &embedder_id)?;
            }
            OnModelChange::ReindexBackground => {
                return Err(MemoryError::EmbedderDimensionMismatch { stored, configured });
            }

            OnModelChange::WipeAndRebuild => {
                let dropped = index.wipe_and_reopen(&tenant, &embedder_id)?;
                index.enqueue_backlog_from_content(&tenant)?;
                index.record_embedder_log(&tenant, &embedder_id, dropped, "wipe_and_rebuild")?;
            }
        }
    }
    Err(e) => return Err(e),
}
```

Bootstrap then proceeds normally. `BackgroundEmbedder` is spawned and its backlog drainer picks up the work asynchronously.

### New `VectorIndex` trait methods

- `enqueue_backlog_from_chunks(tenant) -> Result<u64, MemoryError>` — one row per `(path, version)` from `memory_chunks`. Idempotent via `INSERT OR IGNORE`.
- `enqueue_backlog_from_content(tenant) -> Result<u64, MemoryError>` — one row per `(path, version)` from `memory_content`.
- `wipe_and_reopen(tenant, new_embedder_id) -> Result<u64, MemoryError>` — drop + recreate `memory_vectors` (virtual table, new dim) and `memory_chunks`, update `memory_schema_meta` with `embedder_id` + `dim`. Returns count of cleared chunks.
- `set_embedder_id(tenant, id) -> Result<(), MemoryError>` — updates `memory_schema_meta` after same-dim reindex.
- `take_backlog_batch(tenant, batch_size) -> Result<Vec<BacklogRow>, MemoryError>` — worker uses this to pull up to N oldest rows.
- `delete_backlog_row(tenant, path, version) -> Result<(), MemoryError>` — worker uses this after successful re-embed.
- `bump_backlog_retry(tenant, path, version, err) -> Result<(), MemoryError>` — worker uses this on failure.
- `record_embedder_log(tenant, id, count, action)` — already exists; verify wiring.

`backlog_count` already exists and is used by the `simulacra_memory_reindex_backlog` gauge.

### Backlog-draining worker (`simulacra-memory::background`)

A second per-tenant task alongside the existing Put-event worker. Shares the embedder handle; does not share a queue with the Put worker.

```text
loop {
    rows = index.take_backlog_batch(tenant, batch_size)
    if rows.is_empty() {
        sleep(drain_idle_interval)   // 5s
        continue
    }
    for (path, version) in rows {
        process_backlog_row(tenant, path, version) match {
            Ok(()) => index.delete_backlog_row(...)
            Err(e) => index.bump_backlog_retry(..., &e)
        }
    }
}
```

`process_backlog_row(path, version)`:
```text
chunks = index.load_chunks(tenant, path, version)
if chunks.is_empty() {
    // wipe_and_rebuild path: re-chunk from content
    content = store.get_content(tenant, path, version)?
    if content.is_none() { return Ok(()) }   // content deleted, drop row
    chunks = chunker.chunk(content.data)
    index.upsert_chunks(tenant, path, version, &chunks)
}
vectors = embedder.embed_batch(chunks.iter().map(|c| &c.text))
index.upsert_vectors(tenant, path, version, &chunks, &vectors)
```

**Failure handling.** `retry_count` is bumped on each failure; after `MAX_RETRIES` (10), the row remains in the backlog with `last_error` set. The `simulacra_memory_reindex_backlog` gauge surfaces steady-state non-zero values for operators to alert on. The backlog is the dead-letter.

**Graceful shutdown.** Shares shutdown signal with the Put worker; exits after the current batch commits.

### Reindexing span attribute (`simulacra-tool::memory`)

`SemanticSearchTool` consults `backlog_count` before the search and records the attr:

```rust
let reindexing = self.index.backlog_count(&tenant).unwrap_or(0) > 0;
span.record("memory.search.reindexing", reindexing);
```

Logged unconditionally (both `true` and `false`) to keep the span schema stable.

Under `reindex_background`, vectors exist but the index is incomplete while the worker drains — `reindexing=true` signals degraded recall. Under `wipe_and_rebuild`, search returns zero hits until the worker makes progress; still correct, still flagged.

## Testing

### Unit tests (`crates/simulacra-memory/tests/`)

- `sqlite_vector_index.rs` additions:
  - `enqueue_backlog_from_chunks` populates one row per (path, version)
  - `enqueue_backlog_from_content` populates one row per content path
  - `wipe_and_reopen` drops + recreates with new dim, updates `memory_schema_meta`, returns cleared count
  - `set_embedder_id` updates meta for same-dim path
- `background_embedder.rs` additions:
  - Seed backlog + swap embedder fake → backlog drains → vectors exist, backlog empty, retry_count not bumped
  - Embedder fake returns error → backlog row remains, retry_count bumped

### Integration tests (`crates/simulacra-memory/tests/model_change_reindex.rs` — new file)

- `same_dim_reindex_background_re_embeds_existing_chunks` (assertion 1140):
  - Seed content with EmbedderA, flush to disk, close
  - Reopen with EmbedderB (same dim, different name), `on_model_change = reindex_background`
  - Assert `mark_tenant_stale` cleared vectors, backlog populated, worker drains, vectors exist with EmbedderB id, backlog empty, `memory_embedder_log` has one `reindex` row
- `different_dim_wipe_and_rebuild_rebuilds_from_content` (assertion 1143):
  - Seed content with EmbedderA (dim N)
  - Reopen with EmbedderC (dim M), `on_model_change = wipe_and_rebuild`
  - Assert chunks dropped and recreated from `memory_content`, vectors have new dim, backlog drained, `memory_embedder_log` has one `wipe_and_rebuild` row
- `different_dim_reindex_background_fails_with_dim_mismatch` (already covered by existing tests — verify still passes)

### Span-attr tests (`crates/simulacra-server/tests/memory_hook_integration.rs`)

- `semantic_search_records_reindexing_true_when_backlog_nonempty` (assertion 1144):
  - Insert a backlog row directly
  - Call the search tool
  - Capture `memory_search` span via `CaptureLayer`
  - Assert `memory.search.reindexing == true`
- `semantic_search_records_reindexing_false_when_backlog_empty` — negative case

### Config tests (`crates/simulacra-config`)

- TOML round-trip: each `on_model_change` variant serializes/deserializes correctly
- Default is `refuse` when field omitted

### Mechanical gate

`cargo build --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` — all four pass.

### Observability validation

`simulacra_memory_reindex_backlog{tenant}` visible in local Obsidian during the reindex integration test — confirms end-to-end metric wiring (per `rules/R010-observability-validation.md`).

## Spec updates (`specs/S037-memory-and-semantic-retrieval.md`)

Flip three assertions to `[x]`:
- 1140 — reindex_background end-to-end
- 1143 — wipe_and_rebuild end-to-end
- 1144 — `memory.search.reindexing=true` span attr

Optional side-effect flip (verify before claiming):
- 1059 — durable overflow→backlog path: this design builds the backlog-draining worker that assertion 1059 also depends on, but 1059 requires the Put-worker's overflow branch to insert into the backlog. That wiring is a small additional change worth considering during implementation — if it falls out naturally, flip; otherwise leave for a follow-up round.

## Non-goals

- **Changing the Put-event hot path.** Put-events still flow through the mpsc queue, not the backlog. This preserves ingest latency.
- **Automatic dead-letter table.** The backlog retains failed rows with `retry_count` and `last_error`. No separate table.
- **Cross-tenant reindex coordination.** Each tenant runs independently.
- **Online dim-change.** Spec is explicit: `reindex_background` cannot change dim. Only `wipe_and_rebuild` handles a dim change.
- **Partial rebuild.** Reindex is always all-or-nothing at the tenant level. No path-level incremental reindex.
