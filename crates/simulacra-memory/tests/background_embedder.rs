use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use simulacra_memory::{
    BACKLOG_MAX_RETRIES, BackgroundEmbedder, BackgroundEmbedderConfig, Chunk, Chunker,
    ChunkerSelector, DEFAULT_ENQUEUE_TIMEOUT_MS, DEFAULT_QUEUE_CAPACITY, DefaultEmbedder, Embedder,
    EmbedderId, IndexedChunk, MemoryError, MemoryStore, SearchHit, SqliteMemoryStore,
    SqliteVectorIndex, VectorIndex,
};
use simulacra_types::{Locator, MemoryPath, MemoryVersion, TenantId};
use tokio::time::sleep;

fn tenant(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn memory_path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

struct MockChunker {
    calls: Arc<AtomicUsize>,
}

impl MockChunker {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Chunker for MockChunker {
    fn name(&self) -> &str {
        "mock-single-chunk"
    }

    fn chunk(
        &self,
        _source_path: &str,
        content: &[u8],
    ) -> Result<Vec<Chunk>, simulacra_memory::MemoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = std::str::from_utf8(content)
            .map_err(|_| simulacra_memory::MemoryError::Internal("invalid utf-8".to_string()))?
            .to_string();
        Ok(vec![Chunk {
            chunk_index: 0,
            locator: Locator::Text {
                byte_start: 0,
                byte_end: text.len(),
            },
            text,
        }])
    }
}

struct FailingEmbedder {
    id: EmbedderId,
    dim: usize,
}

impl FailingEmbedder {
    fn new(id: EmbedderId) -> Self {
        Self {
            dim: id.dim().unwrap(),
            id,
        }
    }
}

impl Embedder for FailingEmbedder {
    fn id(&self) -> &EmbedderId {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, _chunks: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        Err(MemoryError::Internal(
            "failing embedder always errors".to_string(),
        ))
    }
}

struct Harness {
    temp: tempfile::TempDir,
    store: Arc<SqliteMemoryStore>,
    index: Arc<SqliteVectorIndex>,
    embedder: Arc<DefaultEmbedder>,
}

impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let embedder = Arc::new(DefaultEmbedder::load_default().unwrap());
        let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
        let index = Arc::new(SqliteVectorIndex::new(temp.path(), embedder.id().clone()).unwrap());
        Self {
            temp,
            store,
            index,
            embedder,
        }
    }

    fn spawn(&self, chunker_selector: ChunkerSelector) -> BackgroundEmbedder {
        BackgroundEmbedder::spawn(
            self.store.clone(),
            self.index.clone(),
            self.embedder.clone(),
            chunker_selector,
            BackgroundEmbedderConfig {
                queue_capacity: 8,
                enqueue_timeout: Duration::from_millis(25),
                embed_batch_size: 8,
            },
        )
        .unwrap()
    }
}

fn always_select(chunker: Arc<dyn Chunker>) -> ChunkerSelector {
    Arc::new(move |_| Some(chunker.clone()))
}

fn skip_selected_paths(chunker: Arc<dyn Chunker>) -> ChunkerSelector {
    Arc::new(move |path| {
        if path.as_str().ends_with(".skip") {
            None
        } else {
            Some(chunker.clone())
        }
    })
}

fn tenant_db_path(root: &Path, tenant: &TenantId) -> PathBuf {
    root.join("memory")
        .join(format!("{}.db", tenant.as_fs_segment()))
}

/// Reads a single row from `memory_embed_backlog` for this path.
/// Returns `(version, retry_count, last_error)`; `None` if no row.
fn backlog_row(
    root: &Path,
    tenant: &TenantId,
    path: &MemoryPath,
) -> Option<(MemoryVersion, u32, Option<String>)> {
    let db_path = tenant_db_path(root, tenant);
    if !db_path.exists() {
        return None;
    }
    let conn = Connection::open(db_path).unwrap();
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='memory_embed_backlog'",
            [],
            |row| row.get(0),
        )
        .ok();
    exists.as_ref()?;
    conn.query_row(
        "SELECT version, retry_count, last_error
           FROM memory_embed_backlog
          WHERE path = ?1",
        params![path.as_str()],
        |row| {
            Ok((
                MemoryVersion(row.get::<_, i64>(0)? as u64),
                row.get::<_, i64>(1)? as u32,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    )
    .ok()
}

/// Chunker that blocks inside `chunk()` until `hold` is released.
/// Increments `entered` each time it's entered, so tests can wait until
/// the tenant worker has actually consumed an event before pushing more
/// Puts to saturate the queue.
struct SaturatingChunker {
    hold: Arc<std::sync::atomic::AtomicBool>,
    entered: Arc<AtomicUsize>,
}

impl Chunker for SaturatingChunker {
    fn name(&self) -> &str {
        "saturating"
    }

    fn chunk(
        &self,
        _source_path: &str,
        content: &[u8],
    ) -> Result<Vec<Chunk>, simulacra_memory::MemoryError> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        while self.hold.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let text = std::str::from_utf8(content)
            .map_err(|_| simulacra_memory::MemoryError::Internal("invalid utf-8".to_string()))?
            .to_string();
        Ok(vec![Chunk {
            chunk_index: 0,
            locator: Locator::Text {
                byte_start: 0,
                byte_end: text.len(),
            },
            text,
        }])
    }
}

/// RAII guard: release the `hold` flag on drop so a test panic or
/// early-return doesn't leave the background worker deadlocked during
/// teardown.
struct ReleaseBoolOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for ReleaseBoolOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn chunk_rows(
    root: &Path,
    tenant: &TenantId,
    path: &MemoryPath,
) -> Vec<(usize, MemoryVersion, String)> {
    let db_path = tenant_db_path(root, tenant);
    if !db_path.exists() {
        return Vec::new();
    }

    let conn = Connection::open(db_path).unwrap();
    // The `memory_chunks` table is created by SqliteVectorIndex on its first
    // `open_conn` call for this tenant. If the background embedder has not
    // yet touched this tenant (e.g., all writes were dedup/skipped), the
    // table may not exist yet — treat that as "no indexed rows".
    let table_exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='memory_chunks'",
            [],
            |row| row.get(0),
        )
        .ok();
    if table_exists.is_none() {
        return Vec::new();
    }

    let mut stmt = conn
        .prepare(
            "SELECT chunk_index, version, text
             FROM memory_chunks
             WHERE path = ?1
             ORDER BY chunk_index ASC",
        )
        .unwrap();

    stmt.query_map(params![path.as_str()], |row| {
        Ok((
            row.get::<_, i64>(0)? as usize,
            MemoryVersion(row.get::<_, i64>(1)? as u64),
            row.get::<_, String>(2)?,
        ))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn search_hits(
    index: &SqliteVectorIndex,
    tenant: &TenantId,
    embedder: &dyn Embedder,
    scope: &MemoryPath,
    query: &str,
) -> Vec<SearchHit> {
    let query_embedding = embedder.embed(&[query]).unwrap().remove(0);
    index
        .search(tenant, scope, &query_embedding, embedder.id(), 10, None)
        .unwrap()
}

async fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if predicate() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_until_worker(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if predicate() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn put_event_is_chunked_embedded_and_upserted_end_to_end() {
    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/notes.md");
    let scope = memory_path("/var/memory/self");
    let version = harness
        .store
        .put(&tenant, &path, b"alpha semantic token")
        .unwrap();

    wait_until("chunk row for put event", || {
        chunk_rows(harness.temp.path(), &tenant, &path)
            == vec![(0, version, "alpha semantic token".to_string())]
    })
    .await;

    let hits = search_hits(
        &harness.index,
        &tenant,
        harness.embedder.as_ref(),
        &scope,
        "alpha semantic token",
    );
    assert!(
        hits.iter()
            .any(|hit| hit.path == path && hit.version == version)
    );
}

#[tokio::test]
async fn delete_event_propagates_into_index_removal() {
    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/delete-me.md");
    let scope = memory_path("/var/memory/self");
    harness
        .store
        .put(&tenant, &path, b"delete target token")
        .unwrap();

    wait_until("initial indexed chunk", || {
        !chunk_rows(harness.temp.path(), &tenant, &path).is_empty()
    })
    .await;

    harness.store.delete(&tenant, &path).unwrap();

    wait_until("chunk removal after delete", || {
        chunk_rows(harness.temp.path(), &tenant, &path).is_empty()
    })
    .await;

    assert!(
        search_hits(
            &harness.index,
            &tenant,
            harness.embedder.as_ref(),
            &scope,
            "delete target token"
        )
        .is_empty()
    );
}

#[tokio::test]
async fn chunker_selector_can_skip_paths_by_returning_none() {
    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(skip_selected_paths(chunker));
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/skip-me.skip");
    let scope = memory_path("/var/memory/self");
    harness
        .store
        .put(&tenant, &path, b"selector skipped token")
        .unwrap();

    sleep(Duration::from_millis(200)).await;

    assert!(chunk_rows(harness.temp.path(), &tenant, &path).is_empty());
    assert!(
        search_hits(
            &harness.index,
            &tenant,
            harness.embedder.as_ref(),
            &scope,
            "selector skipped token"
        )
        .is_empty()
    );
}

#[tokio::test]
async fn dedup_subtree_is_never_indexed_even_when_the_chunker_accepts_it() {
    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/dedup/request-123.txt");
    let scope = memory_path("/var/memory");
    harness
        .store
        .put(&tenant, &path, b"dedup should stay unindexed")
        .unwrap();

    sleep(Duration::from_millis(200)).await;

    assert!(chunk_rows(harness.temp.path(), &tenant, &path).is_empty());
    assert!(
        search_hits(
            &harness.index,
            &tenant,
            harness.embedder.as_ref(),
            &scope,
            "dedup should stay unindexed"
        )
        .is_empty()
    );
}

#[tokio::test]
async fn multiple_tenants_only_see_their_own_indexed_content() {
    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let tenant_a = tenant("tenant-a");
    let tenant_b = tenant("tenant-b");
    let path_a = memory_path("/var/memory/self/a.md");
    let path_b = memory_path("/var/memory/self/b.md");
    let scope = memory_path("/var/memory/self");

    let store_a = harness.store.clone();
    let store_b = harness.store.clone();
    let path_a_clone = path_a.clone();
    let path_b_clone = path_b.clone();
    let put_a = tokio::spawn(async move {
        store_a
            .put(&tenant_a, &path_a_clone, b"tenant_a_unique_token")
            .unwrap();
    });
    let put_b = tokio::spawn(async move {
        store_b
            .put(&tenant_b, &path_b_clone, b"tenant_b_unique_token")
            .unwrap();
    });
    put_a.await.unwrap();
    put_b.await.unwrap();

    wait_until("tenant A indexed", || {
        !chunk_rows(harness.temp.path(), &tenant("tenant-a"), &path_a).is_empty()
    })
    .await;
    wait_until("tenant B indexed", || {
        !chunk_rows(harness.temp.path(), &tenant("tenant-b"), &path_b).is_empty()
    })
    .await;

    let hits_a = search_hits(
        &harness.index,
        &tenant("tenant-a"),
        harness.embedder.as_ref(),
        &scope,
        "tenant_a_unique_token",
    );
    let hits_b = search_hits(
        &harness.index,
        &tenant("tenant-b"),
        harness.embedder.as_ref(),
        &scope,
        "tenant_b_unique_token",
    );

    assert!(hits_a.iter().all(|hit| hit.path == path_a));
    assert!(hits_b.iter().all(|hit| hit.path == path_b));
}

#[tokio::test]
async fn dropping_the_handle_stops_new_events_from_reaching_the_chunker() {
    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let calls = chunker.calls.clone();
    let background = harness.spawn(always_select(chunker));
    let tenant = tenant("tenant-a");
    let first = memory_path("/var/memory/self/before-drop.md");
    let second = memory_path("/var/memory/self/after-drop.md");

    harness.store.put(&tenant, &first, b"before drop").unwrap();
    wait_until("first chunk call", || calls.load(Ordering::SeqCst) == 1).await;

    drop(background);

    harness.store.put(&tenant, &second, b"after drop").unwrap();
    sleep(Duration::from_millis(250)).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(chunk_rows(harness.temp.path(), &tenant, &second).is_empty());
}

#[tokio::test]
async fn rapid_rewrites_leave_only_the_latest_version_searchable() {
    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/rewrite.md");
    let scope = memory_path("/var/memory/self");
    harness
        .store
        .put(&tenant, &path, b"old stale token")
        .unwrap();
    let v2 = harness
        .store
        .put(&tenant, &path, b"new fresh token")
        .unwrap();

    wait_until("latest rewrite indexed", || {
        chunk_rows(harness.temp.path(), &tenant, &path)
            == vec![(0, v2, "new fresh token".to_string())]
    })
    .await;

    // After the rewrite, only the v2 row should exist in memory_chunks for
    // this path — verified by the wait_until above. This proves the index
    // upsert correctly removed v1's chunks before inserting v2's.
    //
    // We do NOT assert that searching for "old stale token" returns empty,
    // because the MVP DefaultEmbedder is a hash-based sketch with weak
    // discriminating power: two different token strings can produce
    // vectors with small but non-zero cosine similarity, so a search at
    // min_cosine=None returns the v2 row for any vaguely similar query.
    // The right behavioral assertion is "v1 content is gone from the
    // chunks table" (already checked above).
    let hits = search_hits(
        &harness.index,
        &tenant,
        harness.embedder.as_ref(),
        &scope,
        "old stale token",
    );
    // Whatever the embedder's scoring says, every returned hit must be at
    // v2 (there's no v1 row anywhere in the index).
    for hit in &hits {
        assert_eq!(hit.version, v2, "no stale v1 chunks survived the rewrite");
        assert_eq!(hit.path, path);
    }
    assert!(
        search_hits(
            &harness.index,
            &tenant,
            harness.embedder.as_ref(),
            &scope,
            "new fresh token"
        )
        .iter()
        .any(|hit| hit.path == path && hit.version == v2)
    );
}

/// S037 §8: the background embedder queue is bounded to 2048 events per
/// tenant by default. This is a contractual constant — callers rely on it
/// for backpressure sizing.
#[test]
fn default_queue_capacity_matches_spec_8() {
    assert_eq!(DEFAULT_QUEUE_CAPACITY, 2048);
    assert_eq!(BackgroundEmbedderConfig::default().queue_capacity, 2048);
}

/// S037 §8: the enqueue timeout is bounded to 100ms under queue pressure.
/// Writes blocked longer than this fall through to the deferred/backlog
/// path so that producers are never held indefinitely.
#[test]
fn default_enqueue_timeout_matches_spec_8() {
    assert_eq!(DEFAULT_ENQUEUE_TIMEOUT_MS, 100);
    assert_eq!(
        BackgroundEmbedderConfig::default().enqueue_timeout,
        Duration::from_millis(100)
    );
}

/// S037 §8: when a tenant's queue is saturated, enqueues must fall through
/// within the configured timeout rather than block the producer
/// indefinitely. We verify by hanging the tenant worker on a slow chunker,
/// flooding the small queue, and asserting that the producer side (the
/// `MemoryStore::put` writer task) continues to accept writes in a bounded
/// window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_stream_does_not_deadlock_when_queue_is_saturated() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration as StdDuration;

    /// A chunker that blocks the worker long enough to guarantee the mpsc
    /// channel fills up while the producer keeps writing.
    struct BlockingChunker {
        hold: Arc<AtomicBool>,
    }

    impl Chunker for BlockingChunker {
        fn name(&self) -> &str {
            "blocking"
        }

        fn chunk(
            &self,
            _source_path: &str,
            content: &[u8],
        ) -> Result<Vec<Chunk>, simulacra_memory::MemoryError> {
            while self.hold.load(Ordering::SeqCst) {
                std::thread::sleep(StdDuration::from_millis(10));
            }
            let text = std::str::from_utf8(content)
                .map_err(|_| simulacra_memory::MemoryError::Internal("invalid utf-8".to_string()))?
                .to_string();
            Ok(vec![Chunk {
                chunk_index: 0,
                locator: Locator::Text {
                    byte_start: 0,
                    byte_end: text.len(),
                },
                text,
            }])
        }
    }

    let harness = Harness::new();
    let hold = Arc::new(AtomicBool::new(true));
    let chunker: Arc<dyn Chunker> = Arc::new(BlockingChunker {
        hold: Arc::clone(&hold),
    });
    let _background = BackgroundEmbedder::spawn(
        harness.store.clone(),
        harness.index.clone(),
        harness.embedder.clone(),
        always_select(chunker),
        BackgroundEmbedderConfig {
            queue_capacity: 4,
            enqueue_timeout: Duration::from_millis(100),
            embed_batch_size: 1,
        },
    )
    .unwrap();

    let tenant = tenant("tenant-pressure");
    let start = Instant::now();

    // Put 20 times into a queue of capacity 4 while the worker is blocked.
    // Every put triggers a MemoryEvent::Put broadcast, which enqueues into
    // the tenant channel. With the worker held, the channel fills; enqueues
    // after that must timeout within 100ms and return `Ok(())` via the
    // deferred path rather than block the MemoryStore::put writer.
    for i in 0..20 {
        let path = memory_path(&format!("/var/memory/self/pressure-{i}.md"));
        harness
            .store
            .put(&tenant, &path, format!("payload {i}").as_bytes())
            .unwrap();
    }

    let elapsed = start.elapsed();
    // Upper bound: 20 events × 100ms timeout = 2s in the extreme case where
    // every enqueue hits the timeout. In practice most events land without
    // timing out. Use a loose 10s ceiling to stay far from flakiness while
    // still catching a true deadlock.
    assert!(
        elapsed < Duration::from_secs(10),
        "put stream blocked for {elapsed:?} under queue pressure — enqueue timeout not bounded"
    );

    // Release the worker so teardown drains cleanly.
    hold.store(false, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlog_worker_drains_backlog_when_chunks_exist() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/backlog-existing.md");
    let scope = memory_path("/var/memory/self");
    let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let new_embedder = Arc::new(DefaultEmbedder::load_default().unwrap());
    let new_id = new_embedder.id().clone();
    let old_id = EmbedderId::new("stale-model", "1.0", new_embedder.dim());
    let old_index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
    let version = store.put(&tenant, &path, b"alpha semantic token").unwrap();
    let stale_embedding = new_embedder
        .embed(&["alpha semantic token"])
        .unwrap()
        .remove(0);
    let stale_chunks = vec![IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text {
            byte_start: 0,
            byte_end: "alpha semantic token".len(),
        },
        text: "alpha semantic token".to_string(),
        embedding: stale_embedding,
    }];

    old_index
        .upsert(&tenant, &path, version, &old_id, &stale_chunks)
        .unwrap();
    assert_eq!(old_index.mark_tenant_stale(&tenant).unwrap(), 1);
    assert_eq!(old_index.enqueue_backlog_from_chunks(&tenant).unwrap(), 1);
    SqliteVectorIndex::set_embedder_id_at(temp.path(), &tenant, &new_id).unwrap();

    let index = Arc::new(SqliteVectorIndex::new(temp.path(), new_id.clone()).unwrap());
    let chunker = Arc::new(MockChunker::new());
    let calls = Arc::clone(&chunker.calls);
    let background = BackgroundEmbedder::spawn(
        store.clone(),
        index.clone(),
        new_embedder.clone(),
        always_select(chunker),
        BackgroundEmbedderConfig::default(),
    )
    .unwrap();

    wait_until_worker("backlog drain for existing chunks", || {
        index.backlog_count(&tenant).unwrap() == 0
    })
    .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "existing chunks should be re-embedded without re-chunking"
    );
    assert!(
        search_hits(
            index.as_ref(),
            &tenant,
            new_embedder.as_ref(),
            &scope,
            "alpha semantic token"
        )
        .iter()
        .any(|hit| hit.path == path && hit.version == version),
        "search should succeed after backlog drain writes vectors"
    );

    background.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlog_worker_rechunks_from_content_when_chunks_absent() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/backlog-rechunk.md");
    let scope = memory_path("/var/memory/self");
    let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let embedder = Arc::new(DefaultEmbedder::load_default().unwrap());
    let index = Arc::new(SqliteVectorIndex::new(temp.path(), embedder.id().clone()).unwrap());
    let version = store
        .put(&tenant, &path, b"rechunk semantic token")
        .unwrap();
    assert_eq!(index.enqueue_backlog_from_content(&tenant).unwrap(), 1);

    let chunker = Arc::new(MockChunker::new());
    let calls = Arc::clone(&chunker.calls);
    let background = BackgroundEmbedder::spawn(
        store.clone(),
        index.clone(),
        embedder.clone(),
        always_select(chunker),
        BackgroundEmbedderConfig::default(),
    )
    .unwrap();

    wait_until_worker("backlog drain for missing chunks", || {
        index.backlog_count(&tenant).unwrap() == 0
    })
    .await;

    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "worker should re-chunk when chunks are absent"
    );
    assert_eq!(
        chunk_rows(temp.path(), &tenant, &path),
        vec![(0, version, "rechunk semantic token".to_string())]
    );
    assert!(
        search_hits(
            index.as_ref(),
            &tenant,
            embedder.as_ref(),
            &scope,
            "rechunk semantic token"
        )
        .iter()
        .any(|hit| hit.path == path && hit.version == version),
        "search should succeed after re-chunking and embedding"
    );

    background.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlog_worker_bumps_retry_on_embedder_failure() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/backlog-failure.md");
    let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let embedder_id = EmbedderId::new("failing-embedder", "1.0", 3);
    let index = Arc::new(SqliteVectorIndex::new(temp.path(), embedder_id.clone()).unwrap());
    store.put(&tenant, &path, b"retry me").unwrap();
    assert_eq!(index.enqueue_backlog_from_content(&tenant).unwrap(), 1);

    let background = BackgroundEmbedder::spawn(
        store,
        index.clone(),
        Arc::new(FailingEmbedder::new(embedder_id)),
        always_select(Arc::new(MockChunker::new())),
        BackgroundEmbedderConfig::default(),
    )
    .unwrap();

    wait_until_worker("backlog retry bump", || {
        matches!(
            backlog_row(temp.path(), &tenant, &path),
            Some((_, retry_count, _)) if retry_count >= 1
        )
    })
    .await;

    let Some((_, retry_count, _)) = backlog_row(temp.path(), &tenant, &path) else {
        panic!("failed row should remain in backlog");
    };
    assert!(
        retry_count >= 1,
        "retry_count should be bumped after embedder failure"
    );

    background.shutdown().await.unwrap();
}

// ── S037 assertion 1059: overflow path → memory_embed_backlog ──────────

/// When the per-tenant dispatch channel is saturated and the enqueue
/// times out, the Put event must be staged in `memory_embed_backlog`
/// rather than silently dropped. The backlog drainer will re-process it.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn overflow_put_writes_to_memory_embed_backlog() {
    let harness = Harness::new();
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let _release_hold = ReleaseBoolOnDrop(Arc::clone(&hold));
    let entered = Arc::new(AtomicUsize::new(0));
    let chunker: Arc<dyn Chunker> = Arc::new(SaturatingChunker {
        hold: Arc::clone(&hold),
        entered: Arc::clone(&entered),
    });
    let background = BackgroundEmbedder::spawn(
        harness.store.clone(),
        harness.index.clone(),
        harness.embedder.clone(),
        always_select(chunker),
        BackgroundEmbedderConfig {
            queue_capacity: 1,
            enqueue_timeout: Duration::from_millis(100),
            embed_batch_size: 1,
        },
    )
    .unwrap();

    let tenant = tenant("tenant-overflow-put");
    let first_path = memory_path("/var/memory/self/overflow-first.md");
    let queued_path = memory_path("/var/memory/self/overflow-queued.md");
    let overflow_path = memory_path("/var/memory/self/overflow-backlog.md");

    harness
        .store
        .put(&tenant, &first_path, b"first blocking content")
        .unwrap();

    wait_until_worker("first put enters blocked chunker", || {
        entered.load(Ordering::SeqCst) >= 1
    })
    .await;

    harness
        .store
        .put(&tenant, &queued_path, b"second queued content")
        .unwrap();
    let overflow_version = harness
        .store
        .put(&tenant, &overflow_path, b"overflow backlog content")
        .unwrap();

    wait_until_worker("overflow put staged in backlog", || {
        harness.index.backlog_count(&tenant).unwrap() == 1
    })
    .await;

    assert_eq!(harness.index.backlog_count(&tenant).unwrap(), 1);
    let batch = harness.index.take_backlog_batch(&tenant, 10).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].path, overflow_path);
    assert_eq!(batch[0].version, overflow_version);

    hold.store(false, Ordering::SeqCst);
    background.shutdown().await.unwrap();
}

/// End-to-end: overflow → backlog → drainer → search. Proves the reaper
/// half of assertion 1059: the backlog row is actually re-queued and
/// produces a search hit once the chunker unblocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn overflow_backlog_row_is_drained_and_search_finds_content() {
    let harness = Harness::new();
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let _release_hold = ReleaseBoolOnDrop(Arc::clone(&hold));
    let entered = Arc::new(AtomicUsize::new(0));
    let chunker: Arc<dyn Chunker> = Arc::new(SaturatingChunker {
        hold: Arc::clone(&hold),
        entered: Arc::clone(&entered),
    });
    let background = BackgroundEmbedder::spawn(
        harness.store.clone(),
        harness.index.clone(),
        harness.embedder.clone(),
        always_select(chunker),
        BackgroundEmbedderConfig {
            queue_capacity: 1,
            enqueue_timeout: Duration::from_millis(100),
            embed_batch_size: 1,
        },
    )
    .unwrap();

    let tenant = tenant("tenant-overflow-drain");
    let scope = memory_path("/var/memory/self");
    let first_path = memory_path("/var/memory/self/drain-first.md");
    let queued_path = memory_path("/var/memory/self/drain-queued.md");
    let overflow_path = memory_path("/var/memory/self/drain-overflow.md");

    harness
        .store
        .put(&tenant, &first_path, b"first blocked content")
        .unwrap();

    wait_until_worker("first put enters blocked chunker", || {
        entered.load(Ordering::SeqCst) >= 1
    })
    .await;

    harness
        .store
        .put(&tenant, &queued_path, b"queued sibling content")
        .unwrap();
    let overflow_version = harness
        .store
        .put(
            &tenant,
            &overflow_path,
            b"overflow semantic token unique-to-backlog",
        )
        .unwrap();

    wait_until_worker("overflow put staged in backlog", || {
        harness.index.backlog_count(&tenant).unwrap() == 1
    })
    .await;

    hold.store(false, Ordering::SeqCst);

    wait_until_worker("overflow backlog row drained", || {
        harness.index.backlog_count(&tenant).unwrap() == 0
    })
    .await;

    let hits = search_hits(
        &harness.index,
        &tenant,
        harness.embedder.as_ref(),
        &scope,
        "overflow semantic token unique-to-backlog",
    );
    assert!(
        hits.iter()
            .any(|hit| hit.path == overflow_path && hit.version == overflow_version),
        "search should find the overflowed path after the backlog reaper drains it"
    );

    background.shutdown().await.unwrap();
}

/// An overflowed Delete must still purge chunks from the index so that
/// stale content does not remain searchable under sustained queue
/// saturation. Spec §8 item 4: writes to MemoryStore always succeed;
/// only the indexing fanout can fall behind — and it must eventually
/// catch up. A Delete that silently rides the "dropped" path permanently
/// leaves the prior Put's chunks searchable, violating that invariant.
///
/// Regression for the BLOCKER fix that routes overflowed Delete events
/// through `VectorIndex::delete_path` synchronously from the dispatcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn overflow_delete_removes_search_hits_even_under_saturation() {
    let harness = Harness::new();
    let tenant = tenant("tenant-overflow-delete");
    let scope = memory_path("/var/memory/self");
    let seeded_path = memory_path("/var/memory/self/seeded-target.md");
    let blocked_path = memory_path("/var/memory/self/blocked-first.md");
    let queued_path = memory_path("/var/memory/self/blocked-queued.md");
    let seed_token = "overflow-delete-target-token";

    // Seed chunks directly via the index BEFORE spawning the embedder so
    // we have something for `delete_path` to remove. The content also
    // lands in MemoryStore at a version we can use for the Delete's
    // MemoryEvent.
    let seed_version = harness
        .store
        .put(&tenant, &seeded_path, seed_token.as_bytes())
        .unwrap();
    let seed_embedding = harness.embedder.embed(&[seed_token]).unwrap().remove(0);
    let seeded_chunks = vec![IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text {
            byte_start: 0,
            byte_end: seed_token.len(),
        },
        text: seed_token.to_string(),
        embedding: seed_embedding,
    }];
    harness
        .index
        .upsert(
            &tenant,
            &seeded_path,
            seed_version,
            harness.embedder.id(),
            &seeded_chunks,
        )
        .unwrap();

    // Sanity: the seeded path is searchable before we overflow the queue.
    assert!(
        search_hits(
            &harness.index,
            &tenant,
            harness.embedder.as_ref(),
            &scope,
            seed_token,
        )
        .iter()
        .any(|hit| hit.path == seeded_path && hit.version == seed_version),
        "seed failed — cannot prove delete overflow without a starting hit",
    );

    // Spawn an embedder with a blocked chunker so the tenant worker
    // wedges on the first Put and subsequent events stack up in the
    // tiny queue.
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let _release_hold = ReleaseBoolOnDrop(Arc::clone(&hold));
    let entered = Arc::new(AtomicUsize::new(0));
    let chunker: Arc<dyn Chunker> = Arc::new(SaturatingChunker {
        hold: Arc::clone(&hold),
        entered: Arc::clone(&entered),
    });
    let background = BackgroundEmbedder::spawn(
        harness.store.clone(),
        harness.index.clone(),
        harness.embedder.clone(),
        always_select(chunker),
        BackgroundEmbedderConfig {
            queue_capacity: 1,
            enqueue_timeout: Duration::from_millis(100),
            embed_batch_size: 1,
        },
    )
    .unwrap();

    // Saturate the queue: one put enters the blocked chunker, a second
    // put fills the capacity-1 channel.
    harness
        .store
        .put(&tenant, &blocked_path, b"blocking")
        .unwrap();
    wait_until_worker("first put enters blocked chunker", || {
        entered.load(Ordering::SeqCst) >= 1
    })
    .await;
    harness.store.put(&tenant, &queued_path, b"queued").unwrap();

    // Issue a Delete on the seeded path. The worker is blocked and the
    // queue is full, so this Delete event overflows. The overflow arm
    // must apply `delete_path` synchronously.
    harness.store.delete(&tenant, &seeded_path).unwrap();

    // Assert the seeded chunks are gone. Poll — the dispatcher applies
    // `delete_path` after the enqueue timeout fires, not instantly.
    wait_until_worker("overflowed delete purges chunks synchronously", || {
        chunk_rows(harness.temp.path(), &tenant, &seeded_path).is_empty()
    })
    .await;

    // And search returns no hits for the seed token.
    let hits = search_hits(
        &harness.index,
        &tenant,
        harness.embedder.as_ref(),
        &scope,
        seed_token,
    );
    assert!(
        !hits.iter().any(|hit| hit.path == seeded_path),
        "overflowed Delete must purge chunks so the content is not searchable; got hits: {hits:?}",
    );

    hold.store(false, Ordering::SeqCst);
    background.shutdown().await.unwrap();
}

/// Unit test for the new `VectorIndex::enqueue_backlog_for` upsert
/// semantics: new rows insert at retry_count=0; newer versions advance
/// and reset retry_count; same or older versions leave the row alone.
#[test]
fn enqueue_backlog_for_advances_version_and_resets_retry() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-enqueue-backlog-for");
    let path = memory_path("/var/memory/self/enqueue-backlog-for.md");
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let index =
        SqliteVectorIndex::new(temp.path(), EmbedderId::new("test-embedder", "1.0", 3)).unwrap();
    let index_api: &dyn VectorIndex = &index;

    let version_1 = store
        .put(&tenant, &path, b"content for backlog row")
        .unwrap();
    assert_eq!(version_1, MemoryVersion(1));
    assert_eq!(index.enqueue_backlog_from_content(&tenant).unwrap(), 1);

    let conn = Connection::open(tenant_db_path(temp.path(), &tenant)).unwrap();
    conn.execute(
        "UPDATE memory_embed_backlog
            SET retry_count = 5,
                last_error = 'still failing'
          WHERE path = ?1",
        params![path.as_str()],
    )
    .unwrap();

    index_api
        .enqueue_backlog_for(&tenant, &path, MemoryVersion(2))
        .unwrap();

    let batch = index.take_backlog_batch(&tenant, 10).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].path, path);
    assert_eq!(batch[0].version, MemoryVersion(2));
    assert_eq!(batch[0].retry_count, 0);

    let row = backlog_row(temp.path(), &tenant, &path).unwrap();
    assert_eq!(row.0, MemoryVersion(2));
    assert_eq!(row.1, 0);
    assert_eq!(row.2, None);

    // Re-enqueue at the same version: row stays as-is.
    index_api
        .enqueue_backlog_for(&tenant, &path, MemoryVersion(2))
        .unwrap();

    let same_version = backlog_row(temp.path(), &tenant, &path).unwrap();
    assert_eq!(same_version.0, MemoryVersion(2));
    assert_eq!(same_version.1, 0);
    assert_eq!(same_version.2, None);

    // Older version is a no-op — the existing row must not regress.
    index_api
        .enqueue_backlog_for(&tenant, &path, MemoryVersion(1))
        .unwrap();

    let older_version = backlog_row(temp.path(), &tenant, &path).unwrap();
    assert_eq!(older_version.0, MemoryVersion(2));
    assert_eq!(older_version.1, 0);
    assert_eq!(older_version.2, None);
}

/// A backlog row that fails `BACKLOG_MAX_RETRIES` times must be
/// dead-lettered — left in the table with retry_count capped — so it
/// stops consuming embedder capacity forever. Verifies the safeguard
/// in `background.rs::backlog_drain_loop` at `row.retry_count >=
/// BACKLOG_MAX_RETRIES`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backlog_dead_letter_row_is_not_retried_past_max_retries() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-dead-letter");
    let path = memory_path("/var/memory/self/dead-letter.md");
    let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let embedder_id = EmbedderId::new("failing-embedder", "1.0", 3);
    let index = Arc::new(SqliteVectorIndex::new(temp.path(), embedder_id.clone()).unwrap());

    let version = store
        .put(&tenant, &path, b"dead letter backlog row")
        .unwrap();
    assert_eq!(index.enqueue_backlog_from_content(&tenant).unwrap(), 1);

    let background = BackgroundEmbedder::spawn(
        store,
        index.clone(),
        Arc::new(FailingEmbedder::new(embedder_id)),
        always_select(Arc::new(MockChunker::new())),
        BackgroundEmbedderConfig::default(),
    )
    .unwrap();

    // The drainer retries every row up to `BACKLOG_MAX_RETRIES` times,
    // bumping `retry_count` on each failure. Once the cap is reached,
    // `take_backlog_batch` filters the row out at the SQL layer — the
    // drainer sees an empty batch and falls into its idle sleep, so the
    // row's retry_count stops advancing. We observe the cap by reading
    // the raw table; `take_backlog_batch` would never surface it.
    wait_until_worker(
        "backlog retry reaches dead-letter cap",
        || match backlog_row(temp.path(), &tenant, &path) {
            Some((_, retry_count, _)) => retry_count >= BACKLOG_MAX_RETRIES,
            None => false,
        },
    )
    .await;

    // Dead-lettered: backlog_count still reports the row so operators
    // see it, but take_backlog_batch (the drainer's view) is empty. If
    // take_backlog_batch still returned the row, the drainer would
    // hot-spin — re-reading the same dead-lettered batch every
    // iteration, setting did_work = true, and never sleeping.
    assert_eq!(index.backlog_count(&tenant).unwrap(), 1);
    assert!(
        index.take_backlog_batch(&tenant, 10).unwrap().is_empty(),
        "dead-lettered rows must be invisible to the drainer",
    );

    let row = backlog_row(temp.path(), &tenant, &path).unwrap();
    assert_eq!(row.0, version);
    assert_eq!(row.1, BACKLOG_MAX_RETRIES);
    assert!(
        row.2.is_some(),
        "dead-letter row should retain the last embedder error for operators"
    );

    // After the cap, the drainer must stop incrementing retry_count so
    // the row doesn't spin forever. Sample the raw row to prove it's
    // pinned at the cap.
    sleep(Duration::from_millis(300)).await;

    let after = backlog_row(temp.path(), &tenant, &path).unwrap();
    assert_eq!(after.0, version);
    assert_eq!(after.1, BACKLOG_MAX_RETRIES);
    assert_eq!(index.backlog_count(&tenant).unwrap(), 1);
    assert!(
        index.take_backlog_batch(&tenant, 10).unwrap().is_empty(),
        "dead-lettered rows must remain invisible to the drainer after the cap",
    );

    background.shutdown().await.unwrap();
}

/// Once dead-lettered, rows must be invisible to the drainer so that
/// `backlog_drain_loop` sees `did_work = false` and falls into its idle
/// sleep. Without this invariant the drainer hot-spins: it reads the same
/// dead-lettered batch every iteration, sets `did_work = true` because the
/// batch is non-empty, and never hits the idle-sleep guard.
///
/// Regression for the BLOCKER fix that moved the `retry_count <
/// BACKLOG_MAX_RETRIES` filter into the SQL layer of
/// `SqliteVectorIndex::take_backlog_batch`.
#[test]
fn take_backlog_batch_excludes_dead_lettered_rows() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-dead-letter-filter");
    let path = memory_path("/var/memory/self/excluded.md");
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let embedder_id = EmbedderId::new("test-embedder", "1.0", 3);
    let index = SqliteVectorIndex::new(temp.path(), embedder_id).unwrap();

    // Stage a fresh backlog row via the normal overflow entry point.
    store.put(&tenant, &path, b"content").unwrap();
    assert_eq!(index.enqueue_backlog_from_content(&tenant).unwrap(), 1);

    // Before pinning at the cap, a fresh row (retry_count = 0) is visible.
    let pre = index.take_backlog_batch(&tenant, 10).unwrap();
    assert_eq!(pre.len(), 1, "fresh row should be visible to the drainer");

    // Pin the row at the dead-letter cap by editing it directly. We write
    // the exact constant the impl checks against so no interpretation of
    // "past the cap" sneaks in — the boundary itself must filter.
    let conn = Connection::open(tenant_db_path(temp.path(), &tenant)).unwrap();
    conn.execute(
        "UPDATE memory_embed_backlog SET retry_count = ?1 WHERE path = ?2",
        params![BACKLOG_MAX_RETRIES as i64, path.as_str()],
    )
    .unwrap();

    // backlog_count (operator-facing) still sees the row.
    assert_eq!(
        index.backlog_count(&tenant).unwrap(),
        1,
        "dead-lettered row must remain in place for operator inspection",
    );

    // take_backlog_batch (drainer-facing) must NOT see it, or the drainer
    // hot-spins on a row it will immediately skip.
    assert!(
        index.take_backlog_batch(&tenant, 10).unwrap().is_empty(),
        "dead-lettered rows must be filtered at the SQL layer",
    );
}
