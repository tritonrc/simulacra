use rusqlite::Connection;
use simulacra_memory::{
    EmbedderId, IndexedChunk, MemoryError, MemoryEvent, MemoryStore, SqliteMemoryStore,
    SqliteVectorIndex, UpsertOutcome, VectorIndex,
};
use simulacra_types::{Locator, MemoryPath, MemoryVersion, TenantId};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Barrier, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, SystemTime};

fn tenant(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn memory_path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

fn db_path(root: &Path, tenant: &TenantId) -> PathBuf {
    root.join("memory")
        .join(format!("{}.db", tenant.as_fs_segment()))
}

fn test_embedder_id() -> EmbedderId {
    EmbedderId::new("test-embedder", "1.0", 3)
}

fn make_store(root: &Path) -> SqliteMemoryStore {
    SqliteMemoryStore::new(root).unwrap()
}

fn seed_schema_meta(root: &Path, tenant: &TenantId) -> EmbedderId {
    let embedder_id = test_embedder_id();
    let index = SqliteVectorIndex::new(root, embedder_id.clone()).unwrap();
    let path = memory_path("/var/memory/self/schema-seed.md");
    let chunks = vec![IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text {
            byte_start: 0,
            byte_end: 4,
        },
        text: "seed".to_string(),
        embedding: vec![1.0, 0.0, 0.0],
    }];

    let outcome = index
        .upsert(tenant, &path, MemoryVersion(1), &embedder_id, &chunks)
        .unwrap();
    assert_eq!(outcome, UpsertOutcome::Applied);

    embedder_id
}

#[test]
fn sqlite_memory_store_put_is_atomic_for_concurrent_readers() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/atomic.txt");
    let old_bytes = vec![b'a'; 8 * 1024];
    let new_bytes = vec![b'b'; 64 * 1024];

    let store = Arc::new(make_store(temp.path()));
    store.put(&tenant, &path, &old_bytes).unwrap();

    let start = Arc::new(Barrier::new(2));
    let done = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));

    let reader = {
        let store: Arc<SqliteMemoryStore> = Arc::clone(&store);
        let tenant = tenant.clone();
        let path = path.clone();
        let start = Arc::clone(&start);
        let done = Arc::clone(&done);
        let observed = Arc::clone(&observed);
        let old_bytes = old_bytes.clone();
        let new_bytes = new_bytes.clone();
        thread::spawn(move || {
            start.wait();
            while !done.load(Ordering::SeqCst) {
                let (bytes, _) = store.get(&tenant, &path).unwrap();
                assert!(
                    bytes == old_bytes || bytes == new_bytes,
                    "concurrent readers must only observe old or new bytes"
                );
                observed.lock().unwrap().push(bytes);
            }
        })
    };

    start.wait();
    let new_version = store.put(&tenant, &path, &new_bytes).unwrap();
    done.store(true, Ordering::SeqCst);
    reader.join().unwrap();

    let (final_bytes, final_version) = store.get(&tenant, &path).unwrap();
    assert_eq!(final_bytes, new_bytes);
    assert_eq!(final_version, new_version);
    assert!(
        !observed.lock().unwrap().is_empty(),
        "the reader thread should observe at least one committed value"
    );
}

#[test]
fn sqlite_memory_store_put_returns_monotonic_versions_per_path() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/monotonic.txt");
    let store = make_store(temp.path());
    let store: &dyn MemoryStore = &store;

    let v1 = store.put(&tenant, &path, b"v1").unwrap();
    let v2 = store.put(&tenant, &path, b"v2").unwrap();
    let v3 = store.put(&tenant, &path, b"v3").unwrap();

    assert!(v1 < v2);
    assert!(v2 < v3);
}

#[test]
fn sqlite_memory_store_delete_bumps_version_and_get_returns_not_found() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/delete-me.txt");
    let store = make_store(temp.path());
    let store: &dyn MemoryStore = &store;

    let live_version = store.put(&tenant, &path, b"hello").unwrap();
    let tombstone_version = store.delete(&tenant, &path).unwrap();

    assert!(tombstone_version > live_version);
    assert!(matches!(
        store.get(&tenant, &path),
        Err(MemoryError::NotFound(_))
    ));
}

#[test]
fn sqlite_memory_store_delete_event_is_visible_on_subscription() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/subscribed.txt");
    let store = make_store(temp.path());
    let mut receiver = store.subscribe().unwrap();

    let live_version = store.put(&tenant, &path, b"hello").unwrap();
    let delete_version = store.delete(&tenant, &path).unwrap();
    assert!(delete_version > live_version);

    let first = receiver.recv_blocking().unwrap();
    let second = receiver.recv_blocking().unwrap();
    let events = [first, second];

    let tenant_ref = &tenant;
    let path_ref = &path;
    let delete_version_ref = &delete_version;
    assert!(events.iter().any(|event| matches!(
        event,
        MemoryEvent::Delete {
            tenant: observed_tenant,
            path: observed_path,
            version,
            ..
        } if observed_tenant == tenant_ref && observed_path == path_ref && version == delete_version_ref
    )));
}

#[test]
fn sqlite_memory_store_list_prefix_returns_metadata_fields() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let prefix = memory_path("/var/memory/self/projects");
    let keep = memory_path("/var/memory/self/projects/roadmap.md");
    let also_keep = memory_path("/var/memory/self/projects/notes/todo.md");
    let outside = memory_path("/var/memory/users/shared.md");
    let store = make_store(temp.path());
    let store: &dyn MemoryStore = &store;
    let before = SystemTime::now();

    let v1 = store.put(&tenant, &keep, b"# roadmap").unwrap();
    let v2 = store.put(&tenant, &also_keep, b"todo item").unwrap();
    store.put(&tenant, &outside, b"outside").unwrap();

    let mut entries = store.list_prefix(&tenant, &prefix).unwrap();
    entries.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));

    // Lexicographic ordering: '/var/memory/self/projects/notes/todo.md' (n)
    // comes before '/var/memory/self/projects/roadmap.md' (r) because 'n' < 'r'.
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, also_keep);
    assert_eq!(entries[0].size, 9);
    assert_eq!(entries[0].version, v2);
    assert!(entries[0].mtime >= before);
    assert_ne!(entries[0].content_hash, [0; 32]);

    assert_eq!(entries[1].path, keep);
    assert_eq!(entries[1].size, 9);
    assert_eq!(entries[1].version, v1);
    assert!(entries[1].mtime >= before);
    assert_ne!(entries[1].content_hash, [0; 32]);
}

#[test]
fn sqlite_memory_store_isolates_two_tenants_that_share_the_same_path() {
    let temp = tempfile::tempdir().unwrap();
    let tenant_a = tenant("tenant-a");
    let tenant_b = tenant("tenant-b");
    let path = memory_path("/var/memory/self/shared-path.md");
    let store_a = make_store(temp.path());
    let store_b = make_store(temp.path());

    store_a.put(&tenant_a, &path, b"tenant a").unwrap();
    store_b.put(&tenant_b, &path, b"tenant b").unwrap();

    assert_eq!(store_a.get(&tenant_a, &path).unwrap().0, b"tenant a");
    assert_eq!(store_b.get(&tenant_b, &path).unwrap().0, b"tenant b");
    assert_ne!(
        db_path(temp.path(), &tenant_a),
        db_path(temp.path(), &tenant_b)
    );
    assert!(db_path(temp.path(), &tenant_a).exists());
    assert!(db_path(temp.path(), &tenant_b).exists());
}

#[test]
fn sqlite_memory_store_concurrent_writes_serialize_and_last_writer_wins() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/serialized.txt");
    let first_bytes = b"first writer".to_vec();
    let second_bytes = b"second writer".to_vec();

    let first_store = Arc::new(make_store(temp.path()));
    let second_store = Arc::new(make_store(temp.path()));
    first_store.put(&tenant, &path, b"seed").unwrap();

    let start = Arc::new(Barrier::new(3));

    let first = {
        let store: Arc<SqliteMemoryStore> = Arc::clone(&first_store);
        let tenant = tenant.clone();
        let path = path.clone();
        let start = Arc::clone(&start);
        let first_bytes = first_bytes.clone();
        thread::spawn(move || {
            start.wait();
            store.put(&tenant, &path, &first_bytes).unwrap()
        })
    };

    let second = {
        let store: Arc<SqliteMemoryStore> = Arc::clone(&second_store);
        let tenant = tenant.clone();
        let path = path.clone();
        let start = Arc::clone(&start);
        let second_bytes = second_bytes.clone();
        thread::spawn(move || {
            start.wait();
            thread::sleep(Duration::from_millis(25));
            store.put(&tenant, &path, &second_bytes).unwrap()
        })
    };

    start.wait();
    let v1 = first.join().unwrap();
    let v2 = second.join().unwrap();

    let (final_bytes, final_version) = first_store.get(&tenant, &path).unwrap();
    assert!(v1 < v2);
    assert_eq!(final_version, v2);
    assert_eq!(final_bytes, second_bytes);
}

#[test]
fn sqlite_memory_store_uses_wal_and_waits_on_busy_connections() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/contention.txt");
    let store = Arc::new(make_store(temp.path()));
    store.put(&tenant, &path, b"seed").unwrap();

    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

    connection.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let started = SystemTime::now();
    let worker = {
        let store: Arc<SqliteMemoryStore> = Arc::clone(&store);
        let tenant = tenant.clone();
        let path = path.clone();
        thread::spawn(move || store.put(&tenant, &path, b"after lock"))
    };

    thread::sleep(Duration::from_millis(150));
    assert!(
        started.elapsed().unwrap() < Duration::from_secs(5),
        "sanity check before releasing the lock"
    );

    thread::sleep(Duration::from_secs(5));
    connection.execute_batch("COMMIT;").unwrap();

    let version = worker.join().unwrap().unwrap();
    assert!(version > MemoryVersion(1));
}

#[test]
fn sqlite_memory_store_records_a_single_memory_schema_meta_row_at_db_creation() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = seed_schema_meta(temp.path(), &tenant);
    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();

    let (row_count, stored_embedder, stored_dim): (i64, String, i64) = connection
        .query_row(
            "SELECT COUNT(*), embedder_id, dim FROM memory_schema_meta;",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert_eq!(row_count, 1);
    assert_eq!(stored_embedder, embedder_id.as_str());
    assert_eq!(stored_dim, 3);
}

#[test]
fn sqlite_memory_store_records_a_dim_that_matches_the_memory_vectors_ddl() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    seed_schema_meta(temp.path(), &tenant);
    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();

    let stored_dim: i64 = connection
        .query_row(
            "SELECT dim FROM memory_schema_meta WHERE id = 1;",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let ddl: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'memory_vectors';",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert!(
        ddl.contains(&format!("FLOAT[{stored_dim}]")),
        "memory_vectors DDL must embed the recorded dimension: {ddl}"
    );
}

#[test]
fn sqlite_memory_store_delete_prefix_removes_all_entries_below_prefix_and_returns_count() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let prefix = memory_path("/var/memory/self/to-delete");
    let inside_a = memory_path("/var/memory/self/to-delete/a.md");
    let inside_b = memory_path("/var/memory/self/to-delete/nested/b.md");
    let outside = memory_path("/var/memory/self/keep.md");
    let store = make_store(temp.path());
    let store: &dyn MemoryStore = &store;

    store.put(&tenant, &inside_a, b"a").unwrap();
    store.put(&tenant, &inside_b, b"b").unwrap();
    store.put(&tenant, &outside, b"keep").unwrap();

    let removed = store.delete_prefix(&tenant, &prefix).unwrap();

    assert_eq!(removed, 2);
    assert!(matches!(
        store.get(&tenant, &inside_a),
        Err(MemoryError::NotFound(_))
    ));
    assert!(matches!(
        store.get(&tenant, &inside_b),
        Err(MemoryError::NotFound(_))
    ));
    assert_eq!(store.get(&tenant, &outside).unwrap().0, b"keep");
}

#[test]
fn sqlite_memory_store_current_version_is_some_for_live_and_none_for_deleted() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let live = memory_path("/var/memory/self/live.md");
    let deleted = memory_path("/var/memory/self/deleted.md");
    let store = make_store(temp.path());
    let store: &dyn MemoryStore = &store;

    let live_version = store.put(&tenant, &live, b"live").unwrap();
    store.put(&tenant, &deleted, b"deleted").unwrap();
    store.delete(&tenant, &deleted).unwrap();

    assert_eq!(
        store.current_version(&tenant, &live).unwrap(),
        Some(live_version)
    );
    assert_eq!(store.current_version(&tenant, &deleted).unwrap(), None);
}
