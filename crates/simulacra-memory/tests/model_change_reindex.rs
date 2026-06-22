//! S037 assertions 1140 + 1143: end-to-end reindex integration tests.
//!
//! - 1140: same dim, different embedder name/version, policy =
//!   `reindex_background`. Startup clears vectors, enqueues the existing
//!   chunks into memory_embed_backlog, flips the meta. The background
//!   worker re-embeds from `memory_chunks.text` under the new embedder.
//! - 1143: different dim, policy = `wipe_and_rebuild`. Startup drops
//!   and recreates vectors+chunks at the new dim, seeds the backlog
//!   from `memory_content`. The background worker re-chunks + re-embeds.
//!
//! Uses the real `SqliteMemoryStore` + `SqliteVectorIndex` + the
//! background embedder. The embedder is a deterministic fake that
//! produces a unit vector keyed on text length — good enough to prove
//! "vectors were written under the new embedder" without requiring a
//! model download.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use simulacra_memory::{
    BackgroundEmbedder, BackgroundEmbedderConfig, Chunk, Chunker, Embedder, EmbedderId,
    IndexedChunk, MemoryError, MemoryStore, OnModelChangePolicy, SqliteMemoryStore,
    SqliteVectorIndex, VectorIndex,
};
use simulacra_types::{Locator, MemoryPath, TenantId};

fn tenant(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

/// Deterministic unit-vector embedder: returns a normalized vector
/// derived from the text's byte content. Two calls with the same text
/// return the same vector. Two embedders with different ids can both
/// produce valid vectors for the same text without requiring a model.
struct FakeEmbedder {
    id: EmbedderId,
    dim: usize,
}

impl FakeEmbedder {
    fn new(name: &str, version: &str, dim: usize) -> Self {
        Self {
            id: EmbedderId::new(name, version, dim),
            dim,
        }
    }
}

impl Embedder for FakeEmbedder {
    fn id(&self) -> &EmbedderId {
        &self.id
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, chunks: &[&str]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let mut out = Vec::with_capacity(chunks.len());
        for (i, text) in chunks.iter().enumerate() {
            // Seed the vector with text-length + chunk-index to avoid
            // all-zero collapse when multiple chunks have the same text.
            let seed = (text.len() as f32) + (i as f32) * 0.1 + 1.0;
            let mut v = vec![0.0_f32; self.dim];
            for (j, entry) in v.iter_mut().enumerate() {
                *entry = (seed + j as f32) * 0.01;
            }
            // Unit-normalize.
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            for entry in v.iter_mut() {
                *entry /= norm;
            }
            out.push(v);
        }
        Ok(out)
    }
}

/// Simple chunker: one chunk per file, whole contents.
struct WholeFileChunker;

impl Chunker for WholeFileChunker {
    fn name(&self) -> &str {
        "whole-file"
    }
    fn chunk(&self, _path: &str, data: &[u8]) -> Result<Vec<Chunk>, MemoryError> {
        let text = String::from_utf8_lossy(data).to_string();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Chunk {
            chunk_index: 0,
            locator: Locator::Text {
                byte_start: 0,
                byte_end: data.len(),
            },
            text,
        }])
    }
}

fn always_chunker() -> simulacra_memory::ChunkerSelector {
    let chunker: Arc<dyn Chunker> = Arc::new(WholeFileChunker);
    Arc::new(move |_| Some(Arc::clone(&chunker)))
}

fn wait_until_backlog_empty(index: &dyn VectorIndex, tenant: &TenantId, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if index.backlog_count(tenant).unwrap() == 0 {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn seeded_old_index(root: &Path, tenant: &TenantId, content: &[(&str, &str)], old_id: EmbedderId) {
    let store = SqliteMemoryStore::new(root).unwrap();
    let old_index = SqliteVectorIndex::new(root, old_id.clone()).unwrap();
    let old_embedder = FakeEmbedder {
        id: old_id.clone(),
        dim: old_id.dim().unwrap(),
    };

    for (path_str, body) in content {
        let p = path(path_str);
        let version = store.put(tenant, &p, body.as_bytes()).unwrap();
        let vec = old_embedder.embed(&[body]).unwrap().remove(0);
        let chunk = IndexedChunk {
            chunk_index: 0,
            locator: Locator::Text {
                byte_start: 0,
                byte_end: body.len(),
            },
            text: body.to_string(),
            embedding: vec,
        };
        old_index
            .upsert(tenant, &p, version, &old_id, std::slice::from_ref(&chunk))
            .unwrap();
    }
}

// S037 1140: same-dim reindex_background end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_dim_reindex_background_re_embeds_existing_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let tenant = tenant("cli");

    // Session 1: seed with EmbedderA (dim 4).
    let old_id = EmbedderId::new("fake", "v1", 4);
    seeded_old_index(
        tmp.path(),
        &tenant,
        &[
            ("/var/memory/self/a.md", "alpha content"),
            ("/var/memory/self/b.md", "beta content"),
        ],
        old_id.clone(),
    );

    // Session 2: configure EmbedderB (same dim, different name/version).
    let new_id = EmbedderId::new("fake", "v2", 4);

    // Startup dispatch with reindex_background.
    simulacra_memory::apply_policy(
        tmp.path(),
        &tenant,
        &new_id,
        OnModelChangePolicy::ReindexBackground,
    )
    .expect("reindex_background policy applied");

    // After dispatch: meta is new_id, vectors cleared, backlog has both paths.
    let index = Arc::new(SqliteVectorIndex::new(tmp.path(), new_id.clone()).unwrap());
    assert_eq!(
        index.backlog_count(&tenant).unwrap(),
        2,
        "both seeded paths should be in the backlog"
    );
    assert_eq!(
        index.embedder_fingerprint(&tenant).unwrap().unwrap(),
        new_id
    );

    // Spawn the background embedder; it drains the backlog.
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(tmp.path()).unwrap());
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", "v2", 4));
    let be = BackgroundEmbedder::spawn(
        Arc::clone(&store),
        Arc::clone(&index) as Arc<dyn VectorIndex>,
        embedder,
        always_chunker(),
        BackgroundEmbedderConfig::default(),
    )
    .unwrap();

    wait_until_backlog_empty(index.as_ref(), &tenant, "reindex_background drain");

    be.shutdown().await.unwrap();

    // Search under the new embedder finds the re-embedded chunks.
    let query_embedder = FakeEmbedder::new("fake", "v2", 4);
    let query = query_embedder.embed(&["alpha content"]).unwrap().remove(0);
    let hits = index
        .search(&tenant, &path("/var/memory/self"), &query, &new_id, 5, None)
        .unwrap();
    assert!(
        !hits.is_empty(),
        "reindex_background should produce searchable vectors under the new embedder"
    );
}

// S037 1143: different-dim wipe_and_rebuild end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_dim_wipe_and_rebuild_rebuilds_from_content() {
    let tmp = tempfile::tempdir().unwrap();
    let tenant = tenant("cli");

    // Session 1: seed with EmbedderA (dim 4).
    let old_id = EmbedderId::new("fake", "v1", 4);
    seeded_old_index(
        tmp.path(),
        &tenant,
        &[("/var/memory/self/a.md", "alpha content")],
        old_id,
    );

    // Session 2: configure EmbedderC (dim 8).
    let new_id = EmbedderId::new("fake", "v3", 8);

    simulacra_memory::apply_policy(
        tmp.path(),
        &tenant,
        &new_id,
        OnModelChangePolicy::WipeAndRebuild,
    )
    .expect("wipe_and_rebuild policy applied");

    let index = Arc::new(SqliteVectorIndex::new(tmp.path(), new_id.clone()).unwrap());
    assert_eq!(
        index.backlog_count(&tenant).unwrap(),
        1,
        "wipe_and_rebuild should seed backlog from memory_content"
    );
    assert_eq!(
        index.embedder_fingerprint(&tenant).unwrap().unwrap(),
        new_id
    );

    // Spawn the background embedder; it re-chunks from content and embeds at dim 8.
    let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(tmp.path()).unwrap());
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder::new("fake", "v3", 8));
    let be = BackgroundEmbedder::spawn(
        Arc::clone(&store),
        Arc::clone(&index) as Arc<dyn VectorIndex>,
        embedder,
        always_chunker(),
        BackgroundEmbedderConfig::default(),
    )
    .unwrap();

    wait_until_backlog_empty(index.as_ref(), &tenant, "wipe_and_rebuild drain");

    be.shutdown().await.unwrap();

    // Search at the new dim finds the rebuilt chunks.
    let query_embedder = FakeEmbedder::new("fake", "v3", 8);
    let query = query_embedder.embed(&["alpha content"]).unwrap().remove(0);
    let hits = index
        .search(&tenant, &path("/var/memory/self"), &query, &new_id, 5, None)
        .unwrap();
    assert!(
        !hits.is_empty(),
        "wipe_and_rebuild should produce searchable vectors at the new dim"
    );
}

// S037 §13: different-dim + reindex_background is rejected.
#[test]
fn different_dim_reindex_background_rejects_with_dim_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let tenant = tenant("cli");
    seeded_old_index(
        tmp.path(),
        &tenant,
        &[("/var/memory/self/a.md", "alpha")],
        EmbedderId::new("fake", "v1", 4),
    );

    let new_id = EmbedderId::new("fake", "v2", 8);
    let err = simulacra_memory::apply_policy(
        tmp.path(),
        &tenant,
        &new_id,
        OnModelChangePolicy::ReindexBackground,
    )
    .expect_err("different dim must be rejected under reindex_background");
    assert!(
        matches!(err, MemoryError::EmbedderDimensionMismatch { .. }),
        "expected EmbedderDimensionMismatch, got {err:?}"
    );
}

// S037 §13: Refuse policy surfaces EmbedderMismatch on same-dim change.
#[test]
fn refuse_policy_surfaces_embedder_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let tenant = tenant("cli");
    seeded_old_index(
        tmp.path(),
        &tenant,
        &[("/var/memory/self/a.md", "alpha")],
        EmbedderId::new("fake", "v1", 4),
    );

    let new_id = EmbedderId::new("fake", "v2", 4);
    let err =
        simulacra_memory::apply_policy(tmp.path(), &tenant, &new_id, OnModelChangePolicy::Refuse)
            .expect_err("Refuse policy must surface mismatch");
    assert!(
        matches!(err, MemoryError::EmbedderMismatch { .. }),
        "expected EmbedderMismatch, got {err:?}"
    );
}

// S037 §13: fresh tenant with any policy is a no-op.
#[test]
fn fresh_tenant_any_policy_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let tenant = tenant("cli");
    let new_id = EmbedderId::new("fake", "v1", 4);

    for policy in [
        OnModelChangePolicy::Refuse,
        OnModelChangePolicy::ReindexBackground,
        OnModelChangePolicy::WipeAndRebuild,
    ] {
        simulacra_memory::apply_policy(tmp.path(), &tenant, &new_id, policy)
            .expect("fresh tenant should be a no-op under every policy");
    }
}
