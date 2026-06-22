use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use simulacra_memory::{
    BackgroundEmbedder, BackgroundEmbedderConfig, Chunk, Chunker, ChunkerSelector, DefaultEmbedder,
    Embedder, MemoryStore, SqliteMemoryStore, SqliteVectorIndex,
};
use simulacra_types::{Locator, MemoryCapability, MemoryPath, TenantId, VfsEvent, VirtualFs};
use simulacra_vfs::{MemoryFs, MemoryStoreFs};
use tokio::time::{sleep, timeout};

fn tenant_id(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn memory_path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

fn capability() -> MemoryCapability {
    MemoryCapability {
        enabled: true,
        search_scopes: vec![memory_path("/var/memory")],
        write_scopes: vec![memory_path("/var/memory")],
    }
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

fn always_select(chunker: Arc<dyn Chunker>) -> ChunkerSelector {
    Arc::new(move |_| Some(chunker.clone()))
}

#[tokio::test]
async fn memory_store_fs_subscribe_emits_written_for_matching_tenant_and_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let vfs = MemoryStoreFs::new(MemoryFs::new(), tenant_id("tenant-a"), store, capability());
    let mut watcher = vfs.subscribe("/var/memory/self");

    vfs.write("/var/memory/self/note.md", b"hello").unwrap();

    let received = timeout(Duration::from_millis(100), watcher.recv()).await;
    assert!(matches!(
        received,
        Ok(Some(VfsEvent::Written { tenant, path, len: 5 }))
            if tenant == tenant_id("tenant-a")
                && path == std::path::Path::new("/var/memory/self/note.md")
    ));
}

#[tokio::test]
async fn memory_store_fs_subscribe_emits_written_then_removed_for_a_write_then_remove() {
    // Drain order: assert Written first, Removed second. Keeps a buggy impl
    // that drops Written and only emits Removed from passing.
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let vfs = MemoryStoreFs::new(MemoryFs::new(), tenant_id("tenant-a"), store, capability());
    let mut watcher = vfs.subscribe("/var/memory/self");

    vfs.write("/var/memory/self/note.md", b"hello").unwrap();
    vfs.remove("/var/memory/self/note.md").unwrap();

    let first = timeout(Duration::from_millis(100), watcher.recv()).await;
    let second = timeout(Duration::from_millis(100), watcher.recv()).await;

    assert!(matches!(
        first,
        Ok(Some(VfsEvent::Written { tenant, path, len: 5 }))
            if tenant == tenant_id("tenant-a")
                && path == std::path::Path::new("/var/memory/self/note.md")
    ));
    assert!(matches!(
        second,
        Ok(Some(VfsEvent::Removed { tenant, path }))
            if tenant == tenant_id("tenant-a")
                && path == std::path::Path::new("/var/memory/self/note.md")
    ));
}

#[tokio::test]
async fn memory_store_fs_subscribe_is_cross_tenant_isolated_on_shared_store() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let tenant_a = MemoryStoreFs::new(
        MemoryFs::new(),
        tenant_id("tenant-a"),
        Arc::clone(&store),
        capability(),
    );
    let tenant_b = MemoryStoreFs::new(
        MemoryFs::new(),
        tenant_id("tenant-b"),
        Arc::clone(&store),
        capability(),
    );
    let mut watcher_a = tenant_a.subscribe("/var/memory/self");
    let mut watcher_b = tenant_b.subscribe("/var/memory/self");

    tenant_a.write("/var/memory/self/a.md", b"a").unwrap();
    tenant_b.write("/var/memory/self/b.md", b"b").unwrap();

    let received_a = timeout(Duration::from_millis(100), watcher_a.recv()).await;
    let received_b = timeout(Duration::from_millis(100), watcher_b.recv()).await;

    assert!(matches!(
        received_a,
        Ok(Some(VfsEvent::Written { tenant, path, .. }))
            if tenant == tenant_id("tenant-a") && path == std::path::Path::new("/var/memory/self/a.md")
    ));
    assert!(matches!(
        received_b,
        Ok(Some(VfsEvent::Written { tenant, path, .. }))
            if tenant == tenant_id("tenant-b") && path == std::path::Path::new("/var/memory/self/b.md")
    ));

    // Non-leakage: after each watcher receives its own tenant's event, the
    // next recv must time out (no cross-tenant event arrives).
    let leak_a = timeout(Duration::from_millis(50), watcher_a.recv()).await;
    let leak_b = timeout(Duration::from_millis(50), watcher_b.recv()).await;
    assert!(
        leak_a.is_err(),
        "tenant-a watcher received cross-tenant event: {leak_a:?}"
    );
    assert!(
        leak_b.is_err(),
        "tenant-b watcher received cross-tenant event: {leak_b:?}"
    );
}

#[tokio::test]
async fn memory_store_fs_subscription_coexists_with_background_embedder() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let embedder = Arc::new(DefaultEmbedder::load_default().unwrap());
    let index = Arc::new(SqliteVectorIndex::new(temp.path(), embedder.id().clone()).unwrap());
    let chunker = Arc::new(MockChunker::new());
    let _background = BackgroundEmbedder::spawn(
        store.clone(),
        index,
        embedder,
        always_select(chunker.clone() as Arc<dyn Chunker>),
        BackgroundEmbedderConfig {
            queue_capacity: 8,
            enqueue_timeout: Duration::from_millis(25),
            embed_batch_size: 1,
        },
    )
    .unwrap();
    let vfs = MemoryStoreFs::new(MemoryFs::new(), tenant_id("tenant-a"), store, capability());
    let mut watcher = vfs.subscribe("/var/memory/self");

    vfs.write("/var/memory/self/coexist.md", b"hello").unwrap();
    sleep(Duration::from_millis(100)).await;

    let received = timeout(Duration::from_millis(100), watcher.recv()).await;
    assert!(matches!(received, Ok(Some(VfsEvent::Written { .. }))));
    assert!(chunker.calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn per_tenant_watchers_do_not_leak_events_when_two_tenants_share_one_store() {
    let temp = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let tenant_a = MemoryStoreFs::new(
        MemoryFs::new(),
        tenant_id("tenant-a"),
        Arc::clone(&store),
        capability(),
    );
    let tenant_b = MemoryStoreFs::new(
        MemoryFs::new(),
        tenant_id("tenant-b"),
        Arc::clone(&store),
        capability(),
    );
    let mut watcher_a = tenant_a.subscribe("/var/memory");
    let mut watcher_b = tenant_b.subscribe("/var/memory");

    tenant_a.write("/var/memory/a.md", b"a").unwrap();
    tenant_b.write("/var/memory/b.md", b"b").unwrap();

    let first_a = timeout(Duration::from_millis(100), watcher_a.recv()).await;
    let first_b = timeout(Duration::from_millis(100), watcher_b.recv()).await;

    assert!(matches!(
        first_a,
        Ok(Some(VfsEvent::Written { tenant, .. })) if tenant == tenant_id("tenant-a")
    ));
    assert!(matches!(
        first_b,
        Ok(Some(VfsEvent::Written { tenant, .. })) if tenant == tenant_id("tenant-b")
    ));

    // Non-leakage: a follow-up recv on each watcher must time out — no
    // cross-tenant event ever arrives.
    let leak_a = timeout(Duration::from_millis(50), watcher_a.recv()).await;
    let leak_b = timeout(Duration::from_millis(50), watcher_b.recv()).await;
    assert!(
        leak_a.is_err(),
        "tenant-a watcher received cross-tenant event: {leak_a:?}"
    );
    assert!(
        leak_b.is_err(),
        "tenant-b watcher received cross-tenant event: {leak_b:?}"
    );
}
