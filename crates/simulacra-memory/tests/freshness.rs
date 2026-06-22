use simulacra_memory::{
    Embedder, EmbedderId, IndexedChunk, MemoryEvent, MemoryEventReceiver, MemoryStore,
    RecentWritesBuffer, SearchHit, SqliteMemoryStore, SqliteVectorIndex, UpsertOutcome,
    VectorIndex,
};
use simulacra_types::{
    Locator, MemoryPath, MemoryVersion, RRWB_MAX_BYTES_PER_ENTRY, RRWB_MAX_ENTRIES, TenantId,
};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct HashEmbedder {
    id: EmbedderId,
    dim: usize,
}

impl HashEmbedder {
    fn new(dim: usize) -> Self {
        Self {
            id: EmbedderId::new("hash-test", "1.0", dim),
            dim,
        }
    }
}

impl Embedder for HashEmbedder {
    fn id(&self) -> &EmbedderId {
        &self.id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, chunks: &[&str]) -> Result<Vec<Vec<f32>>, simulacra_memory::MemoryError> {
        Ok(chunks
            .iter()
            .map(|chunk| {
                let mut raw = Vec::with_capacity(self.dim);
                for index in 0..self.dim {
                    let mut hash = 0xcbf29ce484222325u64;
                    for byte in chunk.as_bytes() {
                        hash ^= u64::from(*byte);
                        hash = hash.wrapping_mul(0x100000001b3);
                    }
                    hash ^= index as u64;
                    hash = hash.wrapping_mul(0x100000001b3);
                    let centered = ((hash % 2001) as f32) - 1000.0;
                    raw.push(centered);
                }

                let norm = raw.iter().map(|value| value * value).sum::<f32>().sqrt();
                raw.into_iter()
                    .map(|value| value / norm)
                    .collect::<Vec<_>>()
            })
            .collect())
    }
}

struct MemoryRuntime<E> {
    root: PathBuf,
    tenant: TenantId,
    store: SqliteMemoryStore,
    index: SqliteVectorIndex,
    receiver: Box<dyn MemoryEventReceiver>,
    rrwb: RecentWritesBuffer,
    embedder: E,
}

impl<E> MemoryRuntime<E>
where
    E: Embedder + Clone,
{
    fn new(root: &Path, tenant: TenantId, embedder: E) -> Self {
        let store = SqliteMemoryStore::new(root).unwrap();
        let index = SqliteVectorIndex::new(root, embedder.id().clone()).unwrap();
        let receiver = store.subscribe().unwrap();

        Self {
            root: root.to_path_buf(),
            tenant,
            store,
            index,
            receiver,
            rrwb: RecentWritesBuffer::new(),
            embedder,
        }
    }

    fn next_run(&self) -> Self {
        Self::new(&self.root, self.tenant.clone(), self.embedder.clone())
    }

    fn write_text(&mut self, path: &MemoryPath, text: &str) -> MemoryVersion {
        let version = self.store.put(&self.tenant, path, text.as_bytes()).unwrap();
        self.rrwb.record(path.clone(), version, text.as_bytes());
        version
    }

    fn delete_path(&mut self, path: &MemoryPath) -> MemoryVersion {
        self.store.delete(&self.tenant, path).unwrap()
    }

    fn search(&self, scope: &MemoryPath, query: &str, k: usize) -> Vec<SearchHit> {
        let mut hits = self.rrwb.search(query, scope);
        let query_embedding = self.embedder.embed(&[query]).unwrap().remove(0);
        hits.extend(
            self.index
                .search(
                    &self.tenant,
                    scope,
                    &query_embedding,
                    self.embedder.id(),
                    k,
                    None,
                )
                .unwrap(),
        );
        hits.sort_by(|left, right| right.cosine_score.total_cmp(&left.cosine_score));
        hits.truncate(k);
        hits
    }

    fn catch_up_one_event(&mut self) {
        let event = self.receiver.recv_blocking().unwrap();
        match event {
            MemoryEvent::Put { path, version, .. } => {
                let (bytes, _) = self.store.get(&self.tenant, &path).unwrap();
                let text = String::from_utf8(bytes).unwrap();
                let embedding = self.embedder.embed(&[text.as_str()]).unwrap().remove(0);
                let chunks = vec![IndexedChunk {
                    chunk_index: 0,
                    locator: Locator::Text {
                        byte_start: 0,
                        byte_end: text.len(),
                    },
                    text,
                    embedding,
                }];

                self.index
                    .upsert(&self.tenant, &path, version, self.embedder.id(), &chunks)
                    .unwrap();
            }
            MemoryEvent::Delete { path, version, .. } => {
                self.index
                    .delete_path(&self.tenant, &path, version)
                    .unwrap();
            }
        }
    }
}

fn tenant(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn memory_path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

#[test]
fn guarantee_one_put_then_get_is_linearizable_within_a_tenant() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/linearizable.md");
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let store: &dyn MemoryStore = &store;

    let version = store.put(&tenant, &path, b"fresh").unwrap();
    let (bytes, observed_version) = store.get(&tenant, &path).unwrap();

    assert_eq!(bytes, b"fresh");
    assert_eq!(observed_version, version);
}

#[test]
fn guarantee_two_small_writes_are_visible_via_read_your_writes_in_the_same_run() {
    let temp = tempfile::tempdir().unwrap();
    let scope = memory_path("/var/memory/self");
    let path = memory_path("/var/memory/self/rrwb.md");
    let mut runtime = MemoryRuntime::new(temp.path(), tenant("tenant-a"), HashEmbedder::new(8));

    runtime.write_text(&path, "needle small write");

    let hits = runtime.search(&scope, "needle", 10);
    assert!(hits.iter().any(|hit| hit.path == path));
}

#[test]
fn guarantee_two_oversized_writes_do_not_get_rrwb_visibility() {
    let temp = tempfile::tempdir().unwrap();
    let scope = memory_path("/var/memory/self");
    let path = memory_path("/var/memory/self/oversized.md");
    let mut runtime = MemoryRuntime::new(temp.path(), tenant("tenant-a"), HashEmbedder::new(8));
    let oversized = format!(
        "oversizedneedle {}",
        "x".repeat(RRWB_MAX_BYTES_PER_ENTRY + 1024)
    );

    runtime.write_text(&path, &oversized);

    let hits = runtime.search(&scope, "oversizedneedle", 10);
    assert!(
        hits.is_empty(),
        "oversized writes are not guaranteed to be visible through the RRWB fast path"
    );
}

#[test]
fn guarantee_two_rrwb_capacity_evicts_the_oldest_entry_after_sixty_five_writes() {
    let temp = tempfile::tempdir().unwrap();
    let scope = memory_path("/var/memory/self");
    let mut runtime = MemoryRuntime::new(temp.path(), tenant("tenant-a"), HashEmbedder::new(8));

    for index in 0..=RRWB_MAX_ENTRIES {
        let path = memory_path(&format!("/var/memory/self/doc-{index}.md"));
        runtime.write_text(&path, &format!("needle-{index}"));
    }

    let oldest_hits = runtime.search(&scope, "needle-0", 10);
    let newest_hits = runtime.search(&scope, &format!("needle-{RRWB_MAX_ENTRIES}"), 10);

    assert!(oldest_hits.is_empty());
    assert!(!newest_hits.is_empty());
}

#[test]
fn guarantee_two_rrwb_isolated_across_runs() {
    let temp = tempfile::tempdir().unwrap();
    let scope = memory_path("/var/memory/self");
    let path = memory_path("/var/memory/self/cross-run.md");
    let mut run_x = MemoryRuntime::new(temp.path(), tenant("tenant-a"), HashEmbedder::new(8));

    run_x.write_text(&path, "runxneedle");

    let run_y = run_x.next_run();
    let hits = run_y.search(&scope, "runxneedle", 10);

    assert!(hits.is_empty());
}

#[test]
fn guarantee_three_after_catch_up_a_new_run_can_search_the_written_content() {
    let temp = tempfile::tempdir().unwrap();
    let scope = memory_path("/var/memory/self");
    let path = memory_path("/var/memory/self/eventual.md");
    let mut run_x = MemoryRuntime::new(temp.path(), tenant("tenant-a"), HashEmbedder::new(8));

    run_x.write_text(&path, "eventualneedle");
    run_x.catch_up_one_event();

    let run_y = run_x.next_run();
    let hits = run_y.search(&scope, "eventualneedle", 10);

    assert!(hits.iter().any(|hit| hit.path == path));
}

#[test]
fn rewrite_visibility_returns_only_the_latest_version_in_search_results() {
    let temp = tempfile::tempdir().unwrap();
    let scope = memory_path("/var/memory/self");
    let path = memory_path("/var/memory/self/rewrite.md");
    let mut run_x = MemoryRuntime::new(temp.path(), tenant("tenant-a"), HashEmbedder::new(8));

    let v1 = run_x.write_text(&path, "topic old-version");
    run_x.catch_up_one_event();
    let v2 = run_x.write_text(&path, "topic new-version");
    run_x.catch_up_one_event();

    assert!(v2 > v1);

    let run_y = run_x.next_run();
    let hits = run_y.search(&scope, "topic", 10);

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, path);
    assert_eq!(hits[0].version, v2);
    assert!(hits[0].snippet.contains("new-version"));
    assert!(!hits[0].snippet.contains("old-version"));
}

#[test]
fn delete_visibility_removes_deleted_content_from_search_results() {
    let temp = tempfile::tempdir().unwrap();
    let scope = memory_path("/var/memory/self");
    let path = memory_path("/var/memory/self/deleted.md");
    let mut run_x = MemoryRuntime::new(temp.path(), tenant("tenant-a"), HashEmbedder::new(8));

    run_x.write_text(&path, "deleteme");
    run_x.catch_up_one_event();
    run_x.delete_path(&path);
    run_x.catch_up_one_event();

    let run_y = run_x.next_run();
    let hits = run_y.search(&scope, "deleteme", 10);

    assert!(hits.is_empty());
}

#[test]
fn stale_upsert_after_delete_does_not_resurrect_deleted_content() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/race.md");
    let embedder = HashEmbedder::new(8);
    let index = SqliteVectorIndex::new(temp.path(), embedder.id().clone()).unwrap();
    let index: &dyn VectorIndex = &index;
    let chunks = vec![IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text {
            byte_start: 0,
            byte_end: 4,
        },
        text: "race".to_string(),
        embedding: embedder.embed(&["race"]).unwrap().remove(0),
    }];

    assert_eq!(
        index
            .upsert(&tenant, &path, MemoryVersion(1), embedder.id(), &chunks)
            .unwrap(),
        UpsertOutcome::Applied
    );
    index.delete_path(&tenant, &path, MemoryVersion(2)).unwrap();
    assert_eq!(
        index
            .upsert(&tenant, &path, MemoryVersion(1), embedder.id(), &chunks)
            .unwrap(),
        UpsertOutcome::Tombstoned
    );

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &embedder.embed(&["race"]).unwrap().remove(0),
            embedder.id(),
            10,
            None,
        )
        .unwrap();
    assert!(hits.is_empty());
}
