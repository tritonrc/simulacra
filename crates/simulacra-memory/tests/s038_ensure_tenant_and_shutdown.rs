use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use simulacra_memory::{
    BackgroundEmbedder, BackgroundEmbedderConfig, Chunk, Chunker, ChunkerSelector, Embedder,
    EmbedderId, MemoryError, MemoryStore, SqliteMemoryStore, SqliteVectorIndex, VectorIndex,
};
use simulacra_types::{Locator, MemoryPath, TenantId};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};

fn tenant(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn memory_path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

fn test_embedder_id() -> EmbedderId {
    EmbedderId::new("s038-test-embedder", "1.0", 3)
}

fn make_store(root: &Path) -> SqliteMemoryStore {
    SqliteMemoryStore::new(root).unwrap()
}

fn make_index(root: &Path, embedder_id: EmbedderId) -> SqliteVectorIndex {
    SqliteVectorIndex::new(root, embedder_id).unwrap()
}

fn always_select(chunker: Arc<dyn Chunker>) -> ChunkerSelector {
    Arc::new(move |_| Some(chunker.clone()))
}

fn table_exists(connection: &Connection, name: &str) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
            params![name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

fn unit_embeddings(dim: usize, count: usize) -> Vec<Vec<f32>> {
    (0..count)
        .map(|_| {
            let mut vector = vec![0.0; dim];
            vector[0] = 1.0;
            vector
        })
        .collect()
}

// SqliteMemoryStore::db_path() is private, so this mirrors the implementation.
fn tenant_db_path(root: &Path, tenant: &TenantId) -> PathBuf {
    root.join("memory")
        .join(format!("{}.db", tenant.as_fs_segment()))
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

struct PassthroughChunker;

impl Chunker for PassthroughChunker {
    fn name(&self) -> &str {
        "passthrough"
    }

    fn chunk(&self, _source_path: &str, content: &[u8]) -> Result<Vec<Chunk>, MemoryError> {
        let text = std::str::from_utf8(content)
            .map_err(|_| MemoryError::Internal("invalid utf-8 test fixture".to_string()))?
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

enum EmbedRule {
    BlockContains {
        needle: &'static str,
        release: Arc<Notify>,
    },
    PanicContains {
        needle: &'static str,
    },
}

struct ScriptedEmbedder {
    id: EmbedderId,
    dim: usize,
    started: AtomicUsize,
    completed: AtomicUsize,
    rules: Vec<EmbedRule>,
}

impl ScriptedEmbedder {
    fn new(rules: Vec<EmbedRule>) -> Self {
        let id = test_embedder_id();
        let dim = id.dim().unwrap();
        Self {
            id,
            dim,
            started: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            rules,
        }
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    fn completed(&self) -> usize {
        self.completed.load(Ordering::SeqCst)
    }
}

impl Embedder for ScriptedEmbedder {
    fn id(&self) -> &EmbedderId {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, chunks: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        self.started.fetch_add(1, Ordering::SeqCst);

        let joined = chunks.join("\n");
        for rule in &self.rules {
            match rule {
                EmbedRule::BlockContains { needle, release } if joined.contains(needle) => {
                    tokio::task::block_in_place(|| {
                        Handle::current().block_on(release.notified());
                    });
                    self.completed.fetch_add(1, Ordering::SeqCst);
                    return Ok(unit_embeddings(self.dim, chunks.len()));
                }
                EmbedRule::PanicContains { needle } if joined.contains(needle) => {
                    panic!("scripted embedder panic for rule {needle}");
                }
                _ => {}
            }
        }

        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(unit_embeddings(self.dim, chunks.len()))
    }
}

struct BackgroundHarness {
    _temp: tempfile::TempDir,
    store: Arc<SqliteMemoryStore>,
    index: Arc<SqliteVectorIndex>,
}

impl BackgroundHarness {
    fn new(embedder_id: EmbedderId) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
        let index = Arc::new(SqliteVectorIndex::new(temp.path(), embedder_id).unwrap());
        Self {
            _temp: temp,
            store,
            index,
        }
    }

    fn spawn(&self, embedder: Arc<dyn Embedder>) -> BackgroundEmbedder {
        BackgroundEmbedder::spawn(
            self.store.clone(),
            self.index.clone(),
            embedder,
            always_select(Arc::new(PassthroughChunker)),
            BackgroundEmbedderConfig {
                queue_capacity: 8,
                enqueue_timeout: Duration::from_millis(25),
                embed_batch_size: 8,
            },
        )
        .unwrap()
    }
}

// S038 AC1: MemoryStore trait gains ensure_tenant(&self, tenant: &TenantId).
#[test]
fn memory_store_trait_exposes_ensure_tenant() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let store = make_store(temp.path());
    let store: &dyn MemoryStore = &store;

    store.ensure_tenant(&tenant).unwrap();
}

// S038 AC2: VectorIndex trait gains ensure_tenant(&self, tenant: &TenantId).
#[test]
fn vector_index_trait_exposes_ensure_tenant() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let index = make_index(temp.path(), test_embedder_id());
    let index: &dyn VectorIndex = &index;

    index.ensure_tenant(&tenant).unwrap();
}

// S038 AC3: SqliteMemoryStore::ensure_tenant opens, migrates, closes, and is idempotent.
#[test]
fn sqlite_memory_store_ensure_tenant_creates_the_tenant_db_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let store = make_store(temp.path());

    store.ensure_tenant(&tenant).unwrap();

    let db_path = tenant_db_path(temp.path(), &tenant);
    assert!(db_path.exists());

    let connection = Connection::open(&db_path).unwrap();
    assert!(table_exists(&connection, "memory_content"));
    let initial_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM memory_content", [], |row| row.get(0))
        .unwrap();
    assert_eq!(initial_rows, 0);
    drop(connection);

    store.ensure_tenant(&tenant).unwrap();

    let connection = Connection::open(&db_path).unwrap();
    assert!(table_exists(&connection, "memory_content"));
    let row_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM memory_content", [], |row| row.get(0))
        .unwrap();
    assert_eq!(row_count, 0);
}

// S038 AC4: SqliteVectorIndex::ensure_tenant opens, migrates, closes, and is idempotent.
#[test]
fn sqlite_vector_index_ensure_tenant_creates_the_tenant_db_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = test_embedder_id();
    let index = make_index(temp.path(), embedder_id.clone());

    index.ensure_tenant(&tenant).unwrap();

    let db_path = tenant_db_path(temp.path(), &tenant);
    assert!(db_path.exists());

    let connection = Connection::open(&db_path).unwrap();
    assert!(table_exists(&connection, "memory_schema_meta"));
    assert!(table_exists(&connection, "memory_chunks"));
    assert!(table_exists(&connection, "memory_vectors"));
    assert!(table_exists(&connection, "memory_embed_backlog"));
    assert!(table_exists(&connection, "memory_embedder_log"));
    assert!(table_exists(&connection, "memory_path_tombstones"));

    let row_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM memory_schema_meta", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(row_count, 1);

    let (stored_embedder, stored_dim): (String, i64) = connection
        .query_row(
            "SELECT embedder_id, dim FROM memory_schema_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored_embedder, embedder_id.as_str());
    assert_eq!(stored_dim, embedder_id.dim().unwrap() as i64);
    drop(connection);

    index.ensure_tenant(&tenant).unwrap();

    let connection = Connection::open(&db_path).unwrap();
    let row_count_after_second_call: i64 = connection
        .query_row("SELECT COUNT(*) FROM memory_schema_meta", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(row_count_after_second_call, 1);
}

// S038 AC5: ensure_tenant on a corrupt sqlite file returns an error.
// Review W4: tighten the assertion to require the error message to
// mention corruption explicitly — sqlite's canonical phrasing is
// "file is not a database" or "database disk image is malformed"
// or similar. Accepting any Sqlite/Io error would let a future bug
// masquerade as the expected failure.
#[test]
fn sqlite_memory_store_ensure_tenant_returns_an_error_for_a_corrupt_tenant_db() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let memory_dir = temp.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();

    let db_path = tenant_db_path(temp.path(), &tenant);
    std::fs::write(&db_path, b"not a sqlite database").unwrap();

    let store = make_store(temp.path());
    let error = store.ensure_tenant(&tenant).unwrap_err();

    let is_corrupt_shaped = matches!(
        error,
        MemoryError::Sqlite(_) | MemoryError::Io(_) | MemoryError::Internal(_)
    );
    let msg = error.to_string().to_lowercase();
    let mentions_corruption = msg.contains("not a database")
        || msg.contains("malformed")
        || msg.contains("corrupt")
        || msg.contains("disk image");

    assert!(
        is_corrupt_shaped,
        "expected a sqlite/io/internal error variant, got {error:?}"
    );
    assert!(
        mentions_corruption,
        "expected the error message to mention sqlite corruption; got: {msg}"
    );
}

// S038 AC6: BackgroundEmbedder::shutdown(self) exists, is async, and consumes self.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_embedder_shutdown_is_an_async_by_value_api() {
    let embedder = Arc::new(ScriptedEmbedder::new(Vec::new()));
    let harness = BackgroundHarness::new(embedder.id().clone());
    let background = harness.spawn(embedder);

    let shutdown = background.shutdown();
    shutdown.await.unwrap();
}

// S038 AC7: BackgroundEmbedder tracks per-tenant worker JoinHandles internally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_embedder_shutdown_waits_for_all_tenant_workers() {
    let release_a = Arc::new(Notify::new());
    let release_b = Arc::new(Notify::new());
    let embedder = Arc::new(ScriptedEmbedder::new(vec![
        EmbedRule::BlockContains {
            needle: "block-tenant-a",
            release: release_a.clone(),
        },
        EmbedRule::BlockContains {
            needle: "block-tenant-b",
            release: release_b.clone(),
        },
    ]));
    let harness = BackgroundHarness::new(embedder.id().clone());
    let background = harness.spawn(embedder.clone());
    let path = memory_path("/var/memory/self/wait-for-all.md");

    harness
        .store
        .put(&tenant("tenant-a"), &path, b"block-tenant-a")
        .unwrap();
    harness
        .store
        .put(&tenant("tenant-b"), &path, b"block-tenant-b")
        .unwrap();

    wait_until("two tenant workers to start", || embedder.started() == 2).await;

    let shutdown = tokio::spawn(async move { background.shutdown().await });

    sleep(Duration::from_millis(100)).await;
    assert!(!shutdown.is_finished());

    release_a.notify_one();
    sleep(Duration::from_millis(100)).await;
    assert!(
        !shutdown.is_finished(),
        "shutdown must still wait for tenant-b's worker handle"
    );

    release_b.notify_one();

    assert!(shutdown.await.unwrap().is_ok());
    assert_eq!(embedder.completed(), 2);
}

// S038 AC8: shutdown stops dispatching, drops senders, waits for workers, and returns Ok.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_embedder_shutdown_stops_dispatching_and_returns_ok_after_workers_exit() {
    let embedder = Arc::new(ScriptedEmbedder::new(Vec::new()));
    let harness = BackgroundHarness::new(embedder.id().clone());
    let background = harness.spawn(embedder.clone());
    let tenant = tenant("tenant-a");
    let before = memory_path("/var/memory/self/before-shutdown.md");
    let after = memory_path("/var/memory/self/after-shutdown.md");

    harness
        .store
        .put(&tenant, &before, b"before shutdown")
        .unwrap();
    wait_until("first embed call", || embedder.started() == 1).await;

    background.shutdown().await.unwrap();

    harness
        .store
        .put(&tenant, &after, b"after shutdown")
        .unwrap();
    sleep(Duration::from_millis(150)).await;

    assert_eq!(embedder.started(), 1);
    assert_eq!(embedder.completed(), 1);
}

// S038 AC9: shutdown returns MemoryError::ShutdownTimeout if a worker exceeds the drain timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_embedder_shutdown_returns_shutdown_timeout_when_a_worker_never_drains() {
    let release = Arc::new(Notify::new());
    let embedder = Arc::new(ScriptedEmbedder::new(vec![EmbedRule::BlockContains {
        needle: "block-forever",
        release: release.clone(),
    }]));
    let harness = BackgroundHarness::new(embedder.id().clone());
    let background = harness.spawn(embedder.clone());
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/timeout.md");

    harness.store.put(&tenant, &path, b"block-forever").unwrap();
    wait_until("blocked worker to start", || embedder.started() == 1).await;

    let shutdown_result = timeout(Duration::from_secs(35), background.shutdown())
        .await
        .expect("shutdown should return before the outer test timeout");

    assert!(matches!(shutdown_result, Err(MemoryError::ShutdownTimeout)));
}

// S038 AC10: shutdown returns WorkerPanic { tenant } while still draining the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_embedder_shutdown_reports_the_panicked_tenant_after_draining_the_rest() {
    let release = Arc::new(Notify::new());
    let embedder = Arc::new(ScriptedEmbedder::new(vec![
        EmbedRule::PanicContains {
            needle: "panic-tenant-a",
        },
        EmbedRule::BlockContains {
            needle: "block-tenant-b",
            release: release.clone(),
        },
    ]));
    let harness = BackgroundHarness::new(embedder.id().clone());
    let background = harness.spawn(embedder.clone());
    let panic_tenant = tenant("tenant-a");
    let other_tenant = tenant("tenant-b");
    let path = memory_path("/var/memory/self/panic.md");

    harness
        .store
        .put(&panic_tenant, &path, b"panic-tenant-a")
        .unwrap();
    harness
        .store
        .put(&other_tenant, &path, b"block-tenant-b")
        .unwrap();

    wait_until("panic and blocking workers to start", || {
        embedder.started() == 2
    })
    .await;

    let shutdown = tokio::spawn(async move { background.shutdown().await });

    sleep(Duration::from_millis(100)).await;
    release.notify_one();

    let result = shutdown.await.unwrap();
    match result {
        Err(MemoryError::WorkerPanic {
            tenant: observed_tenant,
        }) => {
            assert_eq!(observed_tenant, panic_tenant);
        }
        other => panic!("expected WorkerPanic for tenant-a, got {other:?}"),
    }
    assert_eq!(
        embedder.completed(),
        1,
        "shutdown should still drain the non-panicking tenant worker"
    );
}

// S038 AC11: shutdown waits for in-flight embed work to finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_embedder_shutdown_waits_for_in_flight_embed_work() {
    let release = Arc::new(Notify::new());
    let embedder = Arc::new(ScriptedEmbedder::new(vec![EmbedRule::BlockContains {
        needle: "hold-open",
        release: release.clone(),
    }]));
    let harness = BackgroundHarness::new(embedder.id().clone());
    let background = harness.spawn(embedder.clone());
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/in-flight.md");

    harness.store.put(&tenant, &path, b"hold-open").unwrap();
    wait_until("in-flight embed work to start", || embedder.started() == 1).await;

    let shutdown = tokio::spawn(async move { background.shutdown().await });

    sleep(Duration::from_millis(100)).await;
    assert!(
        !shutdown.is_finished(),
        "shutdown must wait for the blocked embed() call"
    );

    release.notify_one();

    assert!(shutdown.await.unwrap().is_ok());
    assert_eq!(embedder.completed(), 1);
}

// S038 AC12: shutdown on an idle embedder returns within 1 second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_embedder_shutdown_on_an_idle_embedder_returns_within_one_second() {
    let embedder = Arc::new(ScriptedEmbedder::new(Vec::new()));
    let harness = BackgroundHarness::new(embedder.id().clone());
    let background = harness.spawn(embedder.clone());

    let started_at = Instant::now();
    background.shutdown().await.unwrap();

    assert!(started_at.elapsed() < Duration::from_secs(1));
    assert_eq!(embedder.started(), 0);
    assert_eq!(embedder.completed(), 0);
}

// ─── S038 review reconciliation: drain is load-bearing ─────────────────────
//
// Review B4: the spec's acceptance criteria explicitly demand proof that
// BackgroundEmbedder::shutdown is load-bearing (i.e., persistence breaks
// if shutdown is skipped). The simulacra-cli persistence test was not a valid
// proof because SqliteMemoryStore::put goes to disk synchronously — only
// the VECTOR INDEX upsert depends on the embedder draining.
//
// These two tests wire a scripted embedder that HAS already completed its
// embed() call (so the vector is ready) and then compare "clean shutdown"
// vs "just drop": the former lets the worker finish its index upsert,
// the latter aborts the worker before the upsert reaches sqlite. We
// assert index row counts via the schema-meta table. If a future
// refactor makes drain unnecessary, the negative test fails and the
// regression is visible.

struct DrainHarness {
    _temp: tempfile::TempDir,
    root: PathBuf,
    store: Arc<SqliteMemoryStore>,
    index: Arc<SqliteVectorIndex>,
}

impl DrainHarness {
    fn new(embedder_id: EmbedderId) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let store = Arc::new(SqliteMemoryStore::new(&root).unwrap());
        let index = Arc::new(SqliteVectorIndex::new(&root, embedder_id).unwrap());
        let t = tenant("drain-test");
        store.ensure_tenant(&t).unwrap();
        index.ensure_tenant(&t).unwrap();
        Self {
            _temp: temp,
            root,
            store,
            index,
        }
    }

    fn count_chunks(&self, tenant: &TenantId) -> i64 {
        let db_path = tenant_db_path(&self.root, tenant);
        if !db_path.exists() {
            return 0;
        }
        let conn = Connection::open(&db_path).unwrap();
        conn.query_row("SELECT COUNT(*) FROM memory_chunks", [], |row| row.get(0))
            .unwrap_or(0)
    }
}

// S038 review B4 (positive): clean shutdown drives the worker to completion,
// so the vector index contains the chunk after shutdown returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_is_load_bearing_clean_shutdown_persists_chunks_into_the_index() {
    let embedder = Arc::new(ScriptedEmbedder::new(Vec::new()));
    let harness = DrainHarness::new(embedder.id().clone());
    let tenant_id = tenant("drain-test");
    let path = memory_path("/var/memory/self/drain.md");

    let background = BackgroundEmbedder::spawn(
        harness.store.clone() as Arc<dyn MemoryStore>,
        harness.index.clone() as Arc<dyn VectorIndex>,
        embedder.clone() as Arc<dyn Embedder>,
        always_select(Arc::new(PassthroughChunker)),
        BackgroundEmbedderConfig {
            queue_capacity: 8,
            enqueue_timeout: Duration::from_millis(50),
            embed_batch_size: 8,
        },
    )
    .unwrap();

    harness
        .store
        .put(&tenant_id, &path, b"drain positive test")
        .unwrap();

    // Clean shutdown awaits the worker which completes its upsert.
    background.shutdown().await.unwrap();

    let rows = harness.count_chunks(&tenant_id);
    assert!(
        rows >= 1,
        "clean shutdown must persist the upsert to the vector index; got {rows} rows"
    );
}

// S038 review B4 (negative): dropping the embedder without calling
// shutdown aborts the worker mid-blocked-embed, so the vector index
// does NOT contain the chunk. This is the load-bearing proof: if a
// future refactor makes drain unnecessary, this test fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_is_load_bearing_drop_without_shutdown_loses_in_flight_upsert() {
    use tokio::sync::Notify;
    // Scripted embedder that blocks forever on the barrier. The test
    // drops the BackgroundEmbedder while embed() is still blocked —
    // abort-on-drop cancels the worker before upsert happens.
    let release = Arc::new(Notify::new());
    let embedder = Arc::new(ScriptedEmbedder::new(vec![EmbedRule::BlockContains {
        needle: "drain-negative",
        release: release.clone(),
    }]));
    let harness = DrainHarness::new(embedder.id().clone());
    let tenant_id = tenant("drain-test");
    let path = memory_path("/var/memory/self/drain.md");

    {
        let background = BackgroundEmbedder::spawn(
            harness.store.clone() as Arc<dyn MemoryStore>,
            harness.index.clone() as Arc<dyn VectorIndex>,
            embedder.clone() as Arc<dyn Embedder>,
            always_select(Arc::new(PassthroughChunker)),
            BackgroundEmbedderConfig {
                queue_capacity: 8,
                enqueue_timeout: Duration::from_millis(50),
                embed_batch_size: 8,
            },
        )
        .unwrap();

        harness
            .store
            .put(&tenant_id, &path, b"drain-negative")
            .unwrap();

        // Wait for the worker to actually enter the blocking embed()
        // call. Once started==1 the embed is definitely mid-call but
        // NOT yet mid-upsert.
        wait_until("blocked worker to start", || embedder.started() == 1).await;

        // Drop without calling shutdown. BackgroundEmbedder::drop aborts
        // the dispatcher; workers cancel at their next yield point.
        drop(background);
        // Give the runtime a beat to process the abort. The release
        // notify is NEVER signalled — the worker is stuck in embed()
        // until abort cancels it.
        sleep(Duration::from_millis(100)).await;
    } // background is out of scope, Drop has run.

    let rows = harness.count_chunks(&tenant_id);
    assert_eq!(
        rows, 0,
        "dropping without shutdown must NOT land the in-flight upsert (this test proves drain is load-bearing); got {rows} rows"
    );
}

// ─── S038 review reconciliation: dead-worker eviction ──────────────────────
//
// Review B3: if a worker panics or its channel closes, the cached entry in
// `tenant_workers` must be evicted so subsequent events respawn the worker.
// Without eviction, the tenant is silently bricked until process restart.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_embedder_evicts_dead_worker_and_respawns_on_next_event() {
    // First event triggers a panic via the scripted embedder; second event
    // should respawn the worker because eviction kicked in.
    let embedder = Arc::new(ScriptedEmbedder::new(vec![EmbedRule::PanicContains {
        needle: "panic-payload",
    }]));
    let harness = BackgroundHarness::new(embedder.id().clone());
    let background = harness.spawn(embedder.clone());
    let tenant_id = tenant("tenant-respawn");

    // Event 1: embed panics → worker task exits.
    let path1 = memory_path("/var/memory/self/first.md");
    harness
        .store
        .put(&tenant_id, &path1, b"panic-payload")
        .unwrap();
    wait_until("panicked worker to exit", || embedder.started() >= 1).await;
    // Give the panic propagation a moment so the worker's JoinHandle marks
    // itself finished before the next dispatch_event runs.
    sleep(Duration::from_millis(100)).await;

    // Event 2: non-panicking payload. If eviction works, a new worker is
    // respawned and embed() is called a second time. If eviction does NOT
    // work, the event is silently dropped because the cached sender points
    // at a closed channel, and started() stays at 1.
    let path2 = memory_path("/var/memory/self/second.md");
    harness
        .store
        .put(&tenant_id, &path2, b"healthy-payload")
        .unwrap();

    wait_until("respawned worker to process second event", || {
        embedder.started() >= 2
    })
    .await;

    // Clean shutdown should succeed (the respawned worker has no poisoned
    // state from the panicked predecessor).
    let _ = background.shutdown().await;
    assert!(
        embedder.started() >= 2,
        "dead-worker eviction must respawn on the next event; started={}",
        embedder.started()
    );
}
