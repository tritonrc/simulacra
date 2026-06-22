use rusqlite::Connection;
use simulacra_memory::{
    EmbedderId, IndexedChunk, MemoryError, MemoryStore, SearchHit, SqliteMemoryStore,
    SqliteVectorIndex, UpsertOutcome, VectorIndex,
};
use simulacra_types::{Locator, MemoryPath, MemoryVersion, TenantId};
use std::path::{Path, PathBuf};

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

fn embedder_id() -> EmbedderId {
    EmbedderId::new("test-embedder", "1.0", 3)
}

fn make_index(root: &Path) -> SqliteVectorIndex {
    SqliteVectorIndex::new(root, embedder_id()).unwrap()
}

fn normalize(vector: [f32; 3]) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    vector.into_iter().map(|value| value / norm).collect()
}

fn chunk(chunk_index: usize, text: &str, embedding: Vec<f32>) -> IndexedChunk {
    IndexedChunk {
        chunk_index,
        locator: Locator::Text {
            byte_start: 0,
            byte_end: text.len(),
        },
        text: text.to_string(),
        embedding,
    }
}

fn assert_search_hit_surface_has_no_hit_id(hit: SearchHit) {
    let SearchHit {
        path,
        chunk_index,
        version,
        locator,
        snippet,
        cosine_score,
    } = hit;

    let _ = (path, chunk_index, version, locator, snippet, cosine_score);
}

#[test]
fn sqlite_vector_index_newer_upsert_is_applied_and_replaces_existing_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/doc.md");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let initial = vec![
        chunk(0, "old alpha", normalize([1.0, 0.0, 0.0])),
        chunk(1, "old beta", normalize([0.0, 1.0, 0.0])),
    ];
    let replacement = vec![chunk(0, "new gamma", normalize([1.0, 0.0, 0.0]))];

    assert_eq!(
        index
            .upsert(&tenant, &path, MemoryVersion(1), &embedder_id, &initial)
            .unwrap(),
        UpsertOutcome::Applied
    );
    assert_eq!(
        index
            .upsert(&tenant, &path, MemoryVersion(2), &embedder_id, &replacement)
            .unwrap(),
        UpsertOutcome::Applied
    );

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, path);
    assert_eq!(hits[0].version, MemoryVersion(2));
    assert!(hits[0].snippet.contains("new gamma"));
}

#[test]
fn sqlite_vector_index_stale_upsert_returns_stale_and_leaves_existing_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/doc.md");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(2),
            &embedder_id,
            &[chunk(0, "current", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();

    let outcome = index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "stale", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();

    assert_eq!(outcome, UpsertOutcome::Stale);

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("current"));
    assert_eq!(hits[0].version, MemoryVersion(2));
}

#[test]
fn sqlite_vector_index_upsert_after_tombstone_returns_tombstoned() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/deleted.md");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(2),
            &embedder_id,
            &[chunk(0, "live", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index.delete_path(&tenant, &path, MemoryVersion(3)).unwrap();

    let outcome = index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(3),
            &embedder_id,
            &[chunk(0, "stale resurrection", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();

    assert_eq!(outcome, UpsertOutcome::Tombstoned);
}

#[test]
fn sqlite_vector_index_delete_path_removes_chunks_and_records_tombstone_version() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/path.md");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(1),
            &embedder_id,
            &[
                chunk(0, "first", normalize([1.0, 0.0, 0.0])),
                chunk(1, "second", normalize([0.0, 1.0, 0.0])),
            ],
        )
        .unwrap();

    index.delete_path(&tenant, &path, MemoryVersion(2)).unwrap();

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();
    assert!(hits.is_empty());

    assert_eq!(
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(2),
                &embedder_id,
                &[chunk(
                    0,
                    "same tombstone version",
                    normalize([1.0, 0.0, 0.0])
                )],
            )
            .unwrap(),
        UpsertOutcome::Tombstoned
    );
    assert_eq!(
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(3),
                &embedder_id,
                &[chunk(0, "new after delete", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap(),
        UpsertOutcome::Applied
    );
}

#[test]
fn sqlite_vector_index_delete_prefix_removes_chunks_below_the_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;
    let prefix = memory_path("/var/memory/self/team");

    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/team/a.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "team a", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/team/nested/b.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "team b", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/keep.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "keep", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();

    let removed = index.delete_prefix(&tenant, &prefix).unwrap();
    assert_eq!(removed, 2);

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, memory_path("/var/memory/self/keep.md"));
}

#[test]
fn sqlite_vector_index_search_is_scoped_to_the_requested_tenant() {
    let temp = tempfile::tempdir().unwrap();
    let tenant_a = tenant("tenant-a");
    let tenant_b = tenant("tenant-b");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;
    let path = memory_path("/var/memory/self/shared.md");

    index
        .upsert(
            &tenant_a,
            &path,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "alpha only", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant_b,
            &path,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "beta only", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();

    let hits_a = index
        .search(
            &tenant_a,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();
    let hits_b = index
        .search(
            &tenant_b,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();

    assert_eq!(hits_a.len(), 1);
    assert_eq!(hits_b.len(), 1);
    assert!(hits_a[0].snippet.contains("alpha"));
    assert!(hits_b[0].snippet.contains("beta"));
}

#[test]
fn sqlite_vector_index_search_filters_by_scope_prefix_on_segment_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/a.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "inside", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/selfish/b.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "lookalike", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, memory_path("/var/memory/self/a.md"));
}

#[test]
fn sqlite_vector_index_search_respects_k_and_min_cosine() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/a.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "a", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/b.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "b", normalize([0.8, 0.6, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/c.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "c", normalize([0.0, 1.0, 0.0]))],
        )
        .unwrap();

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            2,
            Some(0.7),
        )
        .unwrap();

    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.cosine_score >= 0.7));
}

#[test]
fn sqlite_vector_index_search_results_are_sorted_by_descending_cosine_score() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/a.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "a", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/b.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "b", normalize([0.8, 0.6, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/c.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "c", normalize([0.0, 1.0, 0.0]))],
        )
        .unwrap();

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();

    let scores = hits
        .iter()
        .map(|hit| hit.cosine_score)
        .collect::<Vec<f32>>();
    let mut sorted = scores.clone();
    sorted.sort_by(|left: &f32, right: &f32| right.total_cmp(left));
    assert_eq!(scores, sorted);
}

#[test]
fn sqlite_vector_index_search_scores_are_mathematical_cosines_in_unit_interval() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/a.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "a", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &memory_path("/var/memory/self/b.md"),
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "b", normalize([-1.0, 0.0, 0.0]))],
        )
        .unwrap();

    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/self"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            10,
            None,
        )
        .unwrap();

    assert!(
        hits.iter()
            .all(|hit| (-1.0..=1.0).contains(&hit.cosine_score))
    );
}

#[test]
fn sqlite_vector_index_search_hit_surface_has_no_hit_id_field() {
    let hit = SearchHit {
        path: memory_path("/var/memory/self/a.md"),
        chunk_index: 0,
        version: MemoryVersion(1),
        locator: Locator::Text {
            byte_start: 0,
            byte_end: 1,
        },
        snippet: "a".to_string(),
        cosine_score: 1.0,
    };

    assert_search_hit_surface_has_no_hit_id(hit);
}

#[test]
fn sqlite_vector_index_embedder_fingerprint_comes_from_memory_schema_meta() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;
    let path = memory_path("/var/memory/self/a.md");

    index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "fingerprint", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();

    assert_eq!(
        index.embedder_fingerprint(&tenant).unwrap(),
        Some(embedder_id)
    );
}

#[test]
fn sqlite_vector_index_mark_tenant_stale_clears_vectors_but_preserves_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;
    let path = memory_path("/var/memory/self/a.md");

    index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(1),
            &embedder_id,
            &[
                chunk(0, "first", normalize([1.0, 0.0, 0.0])),
                chunk(1, "second", normalize([0.0, 1.0, 0.0])),
            ],
        )
        .unwrap();

    let stale_rows = index.mark_tenant_stale(&tenant).unwrap();
    assert_eq!(stale_rows, 2);

    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();
    let chunk_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM memory_chunks;", [], |row| row.get(0))
        .unwrap();
    let vector_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM memory_vectors;", [], |row| row.get(0))
        .unwrap();

    assert_eq!(chunk_rows, 2);
    assert_eq!(vector_rows, 0);
}

#[test]
fn upsert_chunks_only_writes_chunks_without_vectors() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/chunks-only.md");
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let seeded = vec![chunk(0, "alpha", Vec::new()), chunk(1, "beta", Vec::new())];

    index
        .upsert_chunks_only(&tenant, &path, MemoryVersion(7), &seeded)
        .unwrap();

    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();
    let chunk_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM memory_chunks WHERE path = ?1 AND version = ?2",
            rusqlite::params![path.as_str(), 7_i64],
            |row| row.get(0),
        )
        .unwrap();
    let vector_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM memory_vectors", [], |row| row.get(0))
        .unwrap();

    assert_eq!(chunk_rows, 2);
    assert_eq!(vector_rows, 0, "chunks-only path must not write vectors");

    let mut loaded = index
        .load_chunks_for(&tenant, &path, MemoryVersion(7))
        .unwrap();
    loaded.sort_by_key(|chunk| chunk.chunk_index);

    assert_eq!(loaded.len(), seeded.len());
    for (actual, expected) in loaded.iter().zip(seeded.iter()) {
        assert_eq!(actual.chunk_index, expected.chunk_index);
        assert_eq!(actual.locator, expected.locator);
        assert_eq!(actual.text, expected.text);
        assert!(
            actual.embedding.is_empty(),
            "chunks loaded from chunks-only rows must have empty embeddings"
        );
    }
}

#[test]
fn load_chunks_for_returns_empty_on_missing() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/missing.md");
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let loaded = index
        .load_chunks_for(&tenant, &path, MemoryVersion(99))
        .unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn load_chunks_for_roundtrips_through_normal_upsert() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/roundtrip.md");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let seeded = vec![
        chunk(0, "alpha", normalize([1.0, 0.0, 0.0])),
        chunk(1, "beta", normalize([0.0, 1.0, 0.0])),
    ];

    assert_eq!(
        index
            .upsert(&tenant, &path, MemoryVersion(3), &embedder_id, &seeded)
            .unwrap(),
        UpsertOutcome::Applied
    );

    let mut loaded = index
        .load_chunks_for(&tenant, &path, MemoryVersion(3))
        .unwrap();
    loaded.sort_by_key(|chunk| chunk.chunk_index);

    assert_eq!(loaded.len(), seeded.len());
    for (actual, expected) in loaded.iter().zip(seeded.iter()) {
        assert_eq!(actual.chunk_index, expected.chunk_index);
        assert_eq!(actual.locator, expected.locator);
        assert_eq!(actual.text, expected.text);
        assert_eq!(actual.embedding, expected.embedding);
    }
}

#[test]
fn sqlite_vector_index_rejects_non_unit_vectors() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let err = index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "bad", vec![2.0, 0.0, 0.0])],
        )
        .expect_err("non-unit vectors must be rejected");

    assert!(matches!(err, MemoryError::NotUnitVector(_)));
}

// ─── Model-change enforcement (§13) ──────────────────────────────────────────

#[test]
fn sqlite_vector_index_rejects_reopening_with_different_embedder_name() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");

    // First session: write with model "alpha".
    let first_id = EmbedderId::new("alpha", "1.0", 3);
    {
        let index = SqliteVectorIndex::new(temp.path(), first_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &first_id,
                &[chunk(0, "hello", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap();
        // Force per-tenant file + schema_meta row to exist.
        let _ = index
            .search(
                &tenant,
                &memory_path("/var/memory"),
                &normalize([1.0, 0.0, 0.0]),
                &first_id,
                5,
                None,
            )
            .ok();
    }

    // Second session: same dim, different name. MUST fail with EmbedderMismatch.
    let second_id = EmbedderId::new("beta", "1.0", 3);
    let err = SqliteVectorIndex::new(temp.path(), second_id)
        .and_then(|index| {
            // The error may happen at new() OR at first operation on this tenant
            // depending on lazy init. Force tenant DB open either way.
            index
                .search(
                    &tenant,
                    &memory_path("/var/memory"),
                    &normalize([1.0, 0.0, 0.0]),
                    &EmbedderId::new("beta", "1.0", 3),
                    5,
                    None,
                )
                .map(|_| ())
        })
        .expect_err("different embedder name on existing tenant must fail");

    assert!(
        matches!(err, MemoryError::EmbedderMismatch { .. }),
        "expected EmbedderMismatch, got {err:?}"
    );
}

#[test]
fn sqlite_vector_index_rejects_reopening_with_different_dim() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");

    // First session: dim = 3.
    let first_id = EmbedderId::new("alpha", "1.0", 3);
    {
        let index = SqliteVectorIndex::new(temp.path(), first_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &first_id,
                &[chunk(0, "hello", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap();
    }

    // Second session: dim = 4. MUST fail with EmbedderDimensionMismatch
    // (only wipe_and_rebuild can change dim — not in scope here).
    let second_id = EmbedderId::new("alpha", "1.0", 4);
    let err = SqliteVectorIndex::new(temp.path(), second_id.clone())
        .and_then(|index| {
            // Force tenant DB open to trigger the check.
            index
                .search(
                    &tenant,
                    &memory_path("/var/memory"),
                    &[0.5, 0.5, 0.5, 0.5],
                    &second_id,
                    5,
                    None,
                )
                .map(|_| ())
        })
        .expect_err("different dim on existing tenant must fail");

    assert!(
        matches!(err, MemoryError::EmbedderDimensionMismatch { .. }),
        "expected EmbedderDimensionMismatch, got {err:?}"
    );
}

#[test]
fn sqlite_vector_index_same_embedder_reopens_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");
    let embedder_id = embedder_id();

    // First session.
    {
        let index = SqliteVectorIndex::new(temp.path(), embedder_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &embedder_id,
                &[chunk(0, "hello", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap();
    }

    // Second session with the exact same EmbedderId must work and see prior data.
    let index = SqliteVectorIndex::new(temp.path(), embedder_id.clone()).unwrap();
    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory"),
            &normalize([1.0, 0.0, 0.0]),
            &embedder_id,
            5,
            None,
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, path);
}

// ─── delete_prefix resurrection safety ───────────────────────────────────────

#[test]
fn sqlite_vector_index_delete_prefix_writes_tombstones_preventing_resurrection() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let a = memory_path("/var/memory/self/a.md");
    let b = memory_path("/var/memory/self/b.md");
    let outside = memory_path("/var/memory/users/c.md");

    // Populate both prefixes.
    index
        .upsert(
            &tenant,
            &a,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "alpha", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &b,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "beta", normalize([0.0, 1.0, 0.0]))],
        )
        .unwrap();
    index
        .upsert(
            &tenant,
            &outside,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "carol", normalize([0.0, 0.0, 1.0]))],
        )
        .unwrap();

    // Delete the /var/memory/self/ prefix.
    let removed = index
        .delete_prefix(&tenant, &memory_path("/var/memory/self"))
        .unwrap();
    // Count semantics: chunks removed, not paths. We removed 2 chunks (1 per path).
    assert_eq!(removed, 2);

    // A late queued upsert at version 1 for path `a` must be rejected as
    // Tombstoned — the delete_prefix should have written a tombstone at
    // version > 1 for every affected path.
    let outcome = index
        .upsert(
            &tenant,
            &a,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "late alpha", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    assert_eq!(
        outcome,
        UpsertOutcome::Tombstoned,
        "a late upsert at v1 must NOT resurrect content deleted via delete_prefix"
    );

    // The outside path (different prefix) is untouched.
    let hits = index
        .search(
            &tenant,
            &memory_path("/var/memory/users"),
            &normalize([0.0, 0.0, 1.0]),
            &embedder_id,
            5,
            None,
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
}

// S037 §13 / assertion 1140: same-dim reindex_background should enqueue
// one durable backlog row per distinct (path, version) currently in
// memory_chunks.
#[test]
fn enqueue_backlog_from_chunks_writes_one_row_per_path_version() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let path_a = memory_path("/var/memory/self/a.md");
    let path_b = memory_path("/var/memory/self/b.md");
    let chunks = vec![
        chunk(0, "aaaa", normalize([1.0, 0.0, 0.0])),
        chunk(1, "bbbb", normalize([0.0, 1.0, 0.0])),
    ];

    assert_eq!(
        index
            .upsert(&tenant, &path_a, MemoryVersion(1), &embedder_id, &chunks)
            .unwrap(),
        UpsertOutcome::Applied
    );
    assert_eq!(
        index
            .upsert(&tenant, &path_b, MemoryVersion(1), &embedder_id, &chunks)
            .unwrap(),
        UpsertOutcome::Applied
    );

    let enqueued = index.enqueue_backlog_from_chunks(&tenant).unwrap();

    assert_eq!(enqueued, 2, "one row per distinct (path, version)");
    assert_eq!(index.backlog_count(&tenant).unwrap(), 2);
}

#[test]
fn enqueue_backlog_from_chunks_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let path = memory_path("/var/memory/self/a.md");
    let chunks = vec![
        chunk(0, "aaaa", normalize([1.0, 0.0, 0.0])),
        chunk(1, "bbbb", normalize([0.0, 1.0, 0.0])),
    ];

    assert_eq!(
        index
            .upsert(&tenant, &path, MemoryVersion(1), &embedder_id, &chunks)
            .unwrap(),
        UpsertOutcome::Applied
    );

    assert_eq!(
        index.enqueue_backlog_from_chunks(&tenant).unwrap(),
        1,
        "first call enqueues the single (path, version)"
    );
    assert_eq!(
        index.enqueue_backlog_from_chunks(&tenant).unwrap(),
        0,
        "second call is a no-op — row already present"
    );

    assert_eq!(index.backlog_count(&tenant).unwrap(), 1);
}

// S037 §13: tombstoned (delete_path'd) paths are NOT re-enqueued because
// delete_path removes chunks from memory_chunks. Guards against future
// soft-delete refactors silently breaking the invariant.
#[test]
fn enqueue_backlog_from_chunks_skips_deleted_paths() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let path_a = memory_path("/var/memory/self/a.md");
    let path_b = memory_path("/var/memory/self/b.md");
    let chunks = vec![chunk(0, "xxxx", normalize([1.0, 0.0, 0.0]))];

    index
        .upsert(&tenant, &path_a, MemoryVersion(1), &embedder_id, &chunks)
        .unwrap();
    index
        .upsert(&tenant, &path_b, MemoryVersion(1), &embedder_id, &chunks)
        .unwrap();
    index
        .delete_path(&tenant, &path_a, MemoryVersion(2))
        .unwrap();

    let enqueued = index.enqueue_backlog_from_chunks(&tenant).unwrap();
    assert_eq!(enqueued, 1, "only path_b should be enqueued");
    assert_eq!(index.backlog_count(&tenant).unwrap(), 1);
}

// S037 §13 / assertion 1143: wipe_and_rebuild should enqueue one durable
// backlog row per active memory_content path.
#[test]
fn enqueue_backlog_from_content_writes_one_row_per_path() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let store: &dyn MemoryStore = &store;
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let path_a = memory_path("/var/memory/self/a.md");
    let path_b = memory_path("/var/memory/self/b.md");

    store.put(&tenant, &path_a, b"alpha").unwrap();
    store.put(&tenant, &path_b, b"beta").unwrap();

    let enqueued = index.enqueue_backlog_from_content(&tenant).unwrap();

    assert_eq!(enqueued, 2, "one row per active path in memory_content");
    assert_eq!(index.backlog_count(&tenant).unwrap(), 2);
}

#[test]
fn enqueue_backlog_from_content_skips_tombstoned_paths() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let store: &dyn MemoryStore = &store;
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let active = memory_path("/var/memory/self/active.md");
    let tombstoned = memory_path("/var/memory/self/tombstoned.md");

    store.put(&tenant, &active, b"still here").unwrap();
    store.put(&tenant, &tombstoned, b"gone").unwrap();
    store.delete(&tenant, &tombstoned).unwrap();

    let enqueued = index.enqueue_backlog_from_content(&tenant).unwrap();

    assert_eq!(enqueued, 1, "only non-tombstoned paths should be enqueued");
    assert_eq!(index.backlog_count(&tenant).unwrap(), 1);
}

#[test]
fn enqueue_backlog_from_content_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let store: &dyn MemoryStore = &store;
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let path_a = memory_path("/var/memory/self/a.md");
    let path_b = memory_path("/var/memory/self/b.md");

    store.put(&tenant, &path_a, b"alpha").unwrap();
    store.put(&tenant, &path_b, b"beta").unwrap();

    assert_eq!(
        index.enqueue_backlog_from_content(&tenant).unwrap(),
        2,
        "first call enqueues one row per active path"
    );
    assert_eq!(
        index.enqueue_backlog_from_content(&tenant).unwrap(),
        0,
        "second call is a no-op because the backlog rows already exist"
    );

    assert_eq!(index.backlog_count(&tenant).unwrap(), 2);
}

// S037 §13 / assertion 1143: after a path has been `put` multiple times,
// the backlog row stamped by `enqueue_backlog_from_content` must reflect
// the CURRENT version in memory_content, not a stale earlier version.
// Guards against implementations that hardcode version=1 or use MIN().
#[test]
fn enqueue_backlog_from_content_stamps_current_version() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let store: &dyn MemoryStore = &store;
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let path = memory_path("/var/memory/self/a.md");
    store.put(&tenant, &path, b"v1").unwrap();
    store.put(&tenant, &path, b"v2").unwrap();
    let (_, current_version) = store.get(&tenant, &path).unwrap();

    let enqueued = index.enqueue_backlog_from_content(&tenant).unwrap();
    assert_eq!(enqueued, 1);

    let conn = rusqlite::Connection::open(db_path(temp.path(), &tenant)).unwrap();
    let stored_version: i64 = conn
        .query_row(
            "SELECT version FROM memory_embed_backlog WHERE path = ?1",
            rusqlite::params![path.as_str()],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        stored_version as u64, current_version.0,
        "backlog row must be stamped with the current memory_content.version"
    );
}

// S037 §13 / assertion 1143: a path that was previously tombstoned and
// later re-put is live content; enqueue_backlog_from_content must NOT
// be fooled by a stale entry in memory_path_tombstones (owned by the
// index, not the store) into skipping it.
#[test]
fn enqueue_backlog_from_content_enqueues_re_put_path_despite_stale_tombstone() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let embedder_id = embedder_id();
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let store: &dyn MemoryStore = &store;
    let index = make_index(temp.path());
    let index: &dyn VectorIndex = &index;

    let path = memory_path("/var/memory/self/cycled.md");

    // Put → tombstone (via index.delete_path, which writes memory_path_tombstones)
    // → re-put content. The path is now live in memory_content but still has
    // a row in memory_path_tombstones.
    store.put(&tenant, &path, b"v1").unwrap();
    index
        .upsert(
            &tenant,
            &path,
            MemoryVersion(1),
            &embedder_id,
            &[chunk(0, "v1", normalize([1.0, 0.0, 0.0]))],
        )
        .unwrap();
    index.delete_path(&tenant, &path, MemoryVersion(2)).unwrap();
    store.put(&tenant, &path, b"v3").unwrap();

    let enqueued = index.enqueue_backlog_from_content(&tenant).unwrap();
    assert_eq!(
        enqueued, 1,
        "a re-put path must be enqueued even when a stale tombstone row exists"
    );
    assert_eq!(index.backlog_count(&tenant).unwrap(), 1);
}

// S037 §13 / assertion 1143: wipe_and_rebuild drops and recreates
// memory_vectors + memory_chunks with the new dim before reopening.
#[test]
fn wipe_and_reopen_drops_and_recreates_with_new_dim() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");
    let old_id = EmbedderId::new("test-embedder", "1.0", 3);
    let new_id = EmbedderId::new("other", "2.0", 8);

    {
        let index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
        assert_eq!(
            index
                .upsert(
                    &tenant,
                    &path,
                    MemoryVersion(1),
                    &old_id,
                    &[
                        chunk(0, "first", normalize([1.0, 0.0, 0.0])),
                        chunk(1, "second", normalize([0.0, 1.0, 0.0])),
                    ],
                )
                .unwrap(),
            UpsertOutcome::Applied
        );
    }

    let cleared = SqliteVectorIndex::wipe_and_reopen(temp.path(), &tenant, new_id.clone())
        .expect("wipe succeeds");

    assert_eq!(cleared, 2, "returned cleared count matches seeded chunks");

    let reopened = SqliteVectorIndex::new(temp.path(), new_id.clone()).unwrap();
    assert_eq!(
        reopened.embedder_fingerprint(&tenant).unwrap(),
        Some(new_id.clone())
    );

    // Prove the table was actually dropped and recreated at dim 8:
    // a dim-8 upsert must succeed (not fail with a dimension mismatch).
    let reopened: &dyn VectorIndex = &reopened;
    assert_eq!(
        reopened
            .upsert(
                &tenant,
                &path,
                MemoryVersion(2),
                &new_id,
                &[chunk(
                    0,
                    "after wipe",
                    vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                )],
            )
            .unwrap(),
        UpsertOutcome::Applied
    );
}

#[test]
fn wipe_and_reopen_empties_memory_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");
    let old_id = EmbedderId::new("test-embedder", "1.0", 3);
    let new_id = EmbedderId::new("other", "2.0", 8);

    {
        let index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &old_id,
                &[
                    chunk(0, "first", normalize([1.0, 0.0, 0.0])),
                    chunk(1, "second", normalize([0.0, 1.0, 0.0])),
                ],
            )
            .unwrap();
    }

    SqliteVectorIndex::wipe_and_reopen(temp.path(), &tenant, new_id).unwrap();

    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();
    let chunk_rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM memory_chunks;", [], |row| row.get(0))
        .unwrap();

    assert_eq!(chunk_rows, 0);
}

#[test]
fn wipe_and_reopen_updates_schema_meta_to_new_dim_and_id() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");
    let old_id = EmbedderId::new("test-embedder", "1.0", 3);
    let new_id = EmbedderId::new("other", "2.0", 8);

    {
        let index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &old_id,
                &[chunk(0, "seed", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap();
    }

    SqliteVectorIndex::wipe_and_reopen(temp.path(), &tenant, new_id.clone()).unwrap();

    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();
    let (stored_id, stored_dim): (String, i64) = connection
        .query_row(
            "SELECT embedder_id, dim FROM memory_schema_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(stored_id, new_id.as_str());
    assert_eq!(stored_dim, 8);
}

// S037 §13 / copilot BLOCKER closure: wipe_and_reopen must stage the
// rebuild atomically — after wipe, memory_embed_backlog holds one row
// per non-tombstoned memory_content path. Otherwise a crash between
// wipe and enqueue would leave the tenant "silently healthy" with no
// rebuild work queued.
#[test]
fn wipe_and_reopen_seeds_backlog_from_memory_content() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let old_id = EmbedderId::new("test-embedder", "1.0", 3);
    let new_id = EmbedderId::new("other", "2.0", 8);

    // Seed two content paths and one that will be tombstoned.
    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let store: &dyn MemoryStore = &store;
    store
        .put(&tenant, &memory_path("/var/memory/self/a.md"), b"alpha")
        .unwrap();
    store
        .put(&tenant, &memory_path("/var/memory/self/b.md"), b"beta")
        .unwrap();
    store
        .put(
            &tenant,
            &memory_path("/var/memory/self/deleted.md"),
            b"gone",
        )
        .unwrap();
    store
        .delete(&tenant, &memory_path("/var/memory/self/deleted.md"))
        .unwrap();

    // Seed some chunks at old dim.
    {
        let index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &memory_path("/var/memory/self/a.md"),
                MemoryVersion(1),
                &old_id,
                &[chunk(0, "alpha", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap();
    }

    SqliteVectorIndex::wipe_and_reopen(temp.path(), &tenant, new_id.clone()).unwrap();

    // Backlog now holds exactly the 2 non-tombstoned content paths.
    let index = SqliteVectorIndex::new(temp.path(), new_id).unwrap();
    assert_eq!(
        index.backlog_count(&tenant).unwrap(),
        2,
        "wipe must atomically stage backlog from memory_content (non-tombstoned)"
    );
}

// S037 §13: wipe clears stale backlog rows from a prior lifecycle
// before seeding fresh ones, so old entries can't block new seeds via
// INSERT OR IGNORE conflicts.
#[test]
fn wipe_and_reopen_clears_stale_backlog_before_seeding() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let old_id = EmbedderId::new("test-embedder", "1.0", 3);
    let new_id = EmbedderId::new("other", "2.0", 8);

    let store = SqliteMemoryStore::new(temp.path()).unwrap();
    let store: &dyn MemoryStore = &store;
    store
        .put(&tenant, &memory_path("/var/memory/self/a.md"), b"alpha")
        .unwrap();

    // Build up a stale backlog entry pointing at a content path that has
    // since been deleted — this is the "wedge" that copilot flagged.
    {
        let index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
        let index: &dyn VectorIndex = &index;
        index
            .upsert(
                &tenant,
                &memory_path("/var/memory/self/stale.md"),
                MemoryVersion(1),
                &old_id,
                &[chunk(0, "stale", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap();
        index.enqueue_backlog_from_chunks(&tenant).unwrap();
    }
    // "stale.md" is not in memory_content (it was only chunked). After a
    // wipe, the backlog should be reset to reflect memory_content, not
    // carry forward the stale row.

    SqliteVectorIndex::wipe_and_reopen(temp.path(), &tenant, new_id.clone()).unwrap();

    let index = SqliteVectorIndex::new(temp.path(), new_id).unwrap();
    assert_eq!(
        index.backlog_count(&tenant).unwrap(),
        1,
        "stale backlog rows must be cleared; only live memory_content paths remain"
    );
}

#[test]
fn wipe_and_reopen_on_empty_tenant_returns_zero() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");
    let new_id = EmbedderId::new("other", "2.0", 8);

    let cleared = SqliteVectorIndex::wipe_and_reopen(temp.path(), &tenant, new_id.clone())
        .expect("wipe succeeds on empty tenant");
    assert_eq!(cleared, 0);

    let index = SqliteVectorIndex::new(temp.path(), new_id.clone()).unwrap();
    assert_eq!(
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &new_id,
                &[chunk(
                    0,
                    "after wipe",
                    vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                )],
            )
            .unwrap(),
        UpsertOutcome::Applied
    );
}

// S037 1140 / §13: same-dim reindex must update memory_schema_meta.embedder_id
// so a later constructor with the new embedder passes fingerprint validation.
#[test]
fn set_embedder_id_at_updates_schema_meta() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");
    let old_id = embedder_id();
    let new_id = EmbedderId::new("test-embedder", "2.0", 3);

    {
        let index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &old_id,
                &[
                    chunk(0, "first", normalize([1.0, 0.0, 0.0])),
                    chunk(1, "second", normalize([0.0, 1.0, 0.0])),
                ],
            )
            .unwrap();
        assert_eq!(index.mark_tenant_stale(&tenant).unwrap(), 2);
    }

    SqliteVectorIndex::set_embedder_id_at(temp.path(), &tenant, &new_id).unwrap();

    let reopened = SqliteVectorIndex::new(temp.path(), new_id.clone()).unwrap();
    assert_eq!(
        reopened.embedder_fingerprint(&tenant).unwrap(),
        Some(new_id.clone())
    );
}

#[test]
fn set_embedder_id_at_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/self/a.md");
    let old_id = embedder_id();
    let new_id = EmbedderId::new("test-embedder", "2.0", 3);

    {
        let index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &old_id,
                &[chunk(0, "seed", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap();
        assert_eq!(index.mark_tenant_stale(&tenant).unwrap(), 1);
    }

    SqliteVectorIndex::set_embedder_id_at(temp.path(), &tenant, &new_id).unwrap();
    SqliteVectorIndex::set_embedder_id_at(temp.path(), &tenant, &new_id).unwrap();

    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();
    let stored_id: String = connection
        .query_row(
            "SELECT embedder_id FROM memory_schema_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(stored_id, new_id.as_str());
}

#[test]
fn set_embedder_id_at_on_fresh_tenant_seeds_and_reopens_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let new_id = EmbedderId::new("test-embedder", "2.0", 3);

    SqliteVectorIndex::set_embedder_id_at(temp.path(), &tenant, &new_id).unwrap();

    // Reopen with the seeded embedder: fingerprint check passes AND the
    // lazy tenant-open at the first op succeeds.
    let index = SqliteVectorIndex::new(temp.path(), new_id.clone()).unwrap();
    assert_eq!(
        index.embedder_fingerprint(&tenant).unwrap(),
        Some(new_id.clone()),
    );

    // Opening with a DIFFERENT same-dim embedder is rejected once the
    // fingerprint is consulted (on the first tenant op, per the lazy
    // open_conn contract). mark_tenant_stale exercises that path.
    let other_id = EmbedderId::new("other-embedder", "1.0", 3);
    let index = SqliteVectorIndex::new(temp.path(), other_id).unwrap();
    let err = index.mark_tenant_stale(&tenant).unwrap_err();
    assert!(
        matches!(err, MemoryError::EmbedderMismatch { .. }),
        "expected EmbedderMismatch on first tenant op, got {err:?}"
    );
}

// S037 §13: set_embedder_id_at rejects dim changes — those belong to
// wipe_and_reopen. Guards against silent drift where the stored dim
// would point at a vec0 table of a different shape.
#[test]
fn set_embedder_id_at_rejects_dim_change() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let old_id = embedder_id(); // dim 3
    let path = memory_path("/var/memory/self/a.md");

    // Seed schema_meta with dim=3 by performing an actual upsert.
    {
        let index = SqliteVectorIndex::new(temp.path(), old_id.clone()).unwrap();
        index
            .upsert(
                &tenant,
                &path,
                MemoryVersion(1),
                &old_id,
                &[chunk(0, "seed", normalize([1.0, 0.0, 0.0]))],
            )
            .unwrap();
    }

    let different_dim = EmbedderId::new("other", "1.0", 8);
    let err = SqliteVectorIndex::set_embedder_id_at(temp.path(), &tenant, &different_dim)
        .expect_err("dim mismatch must be rejected");
    assert!(
        matches!(err, MemoryError::EmbedderDimensionMismatch { .. }),
        "expected EmbedderDimensionMismatch, got {err:?}"
    );
}

fn seed_backlog_row(
    root: &Path,
    tenant: &TenantId,
    path: &MemoryPath,
    version: MemoryVersion,
    enqueued_at: i64,
    retry_count: u32,
    last_error: Option<&str>,
) {
    let connection = Connection::open(db_path(root, tenant)).unwrap();
    connection
        .execute(
            "INSERT INTO memory_embed_backlog (path, version, enqueued_at, retry_count, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                path.as_str(),
                version.0 as i64,
                enqueued_at,
                retry_count as i64,
                last_error,
            ],
        )
        .unwrap();
}

#[test]
fn take_backlog_batch_returns_rows_oldest_first() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let index = make_index(temp.path());

    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    let first = memory_path("/var/memory/self/first.md");
    let second = memory_path("/var/memory/self/second.md");
    let third = memory_path("/var/memory/self/third.md");

    seed_backlog_row(temp.path(), &tenant, &second, MemoryVersion(2), 20, 0, None);
    seed_backlog_row(temp.path(), &tenant, &third, MemoryVersion(3), 30, 0, None);
    seed_backlog_row(temp.path(), &tenant, &first, MemoryVersion(1), 10, 0, None);

    let batch: Vec<simulacra_memory::BacklogRow> = index.take_backlog_batch(&tenant, 2).unwrap();

    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].path, first);
    assert_eq!(batch[0].version, MemoryVersion(1));
    assert_eq!(batch[0].retry_count, 0);
    assert_eq!(batch[1].path, second);
    assert_eq!(batch[1].version, MemoryVersion(2));
    assert_eq!(batch[1].retry_count, 0);
    let _ = format!("{:?}", batch[0].clone());
}

#[test]
fn delete_backlog_row_removes_exactly_one() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let index = make_index(temp.path());

    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    let first = memory_path("/var/memory/self/first.md");
    let second = memory_path("/var/memory/self/second.md");
    seed_backlog_row(temp.path(), &tenant, &first, MemoryVersion(1), 10, 0, None);
    seed_backlog_row(temp.path(), &tenant, &second, MemoryVersion(2), 20, 0, None);

    let batch = index.take_backlog_batch(&tenant, 2).unwrap();
    assert_eq!(batch.len(), 2);

    index
        .delete_backlog_row(&tenant, &batch[0].path, batch[0].version)
        .unwrap();

    assert_eq!(index.backlog_count(&tenant).unwrap(), 1);

    let remaining = index.take_backlog_batch(&tenant, 10).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].path, batch[1].path);
    assert_eq!(remaining[0].version, batch[1].version);
}

#[test]
fn bump_backlog_retry_increments_retry_count_and_sets_last_error() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let index = make_index(temp.path());

    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    let path = memory_path("/var/memory/self/retry.md");
    seed_backlog_row(temp.path(), &tenant, &path, MemoryVersion(7), 10, 0, None);

    let batch = index.take_backlog_batch(&tenant, 1).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].retry_count, 0);

    index
        .bump_backlog_retry(&tenant, &batch[0].path, batch[0].version, "embedder err")
        .unwrap();

    let bumped = index.take_backlog_batch(&tenant, 1).unwrap();
    assert_eq!(bumped.len(), 1);
    assert_eq!(bumped[0].path, path);
    assert_eq!(bumped[0].version, MemoryVersion(7));
    assert_eq!(bumped[0].retry_count, 1);

    let connection = Connection::open(db_path(temp.path(), &tenant)).unwrap();
    let last_error: String = connection
        .query_row(
            "SELECT last_error FROM memory_embed_backlog WHERE path = ?1 AND version = ?2",
            rusqlite::params![path.as_str(), 7_i64],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(last_error, "embedder err");
}

#[test]
fn take_backlog_batch_deprioritizes_rows_with_high_retry_count() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let index = make_index(temp.path());

    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    let preferred = memory_path("/var/memory/self/preferred.md");
    let retried = memory_path("/var/memory/self/retried.md");
    seed_backlog_row(
        temp.path(),
        &tenant,
        &preferred,
        MemoryVersion(1),
        20,
        0,
        None,
    );
    seed_backlog_row(
        temp.path(),
        &tenant,
        &retried,
        MemoryVersion(2),
        10,
        0,
        None,
    );

    for _ in 0..5 {
        index
            .bump_backlog_retry(&tenant, &retried, MemoryVersion(2), "still failing")
            .unwrap();
    }

    let batch = index.take_backlog_batch(&tenant, 1).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].path, preferred);
    assert_eq!(batch[0].retry_count, 0);
}

#[test]
fn delete_backlog_row_on_missing_row_is_not_error() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let index = make_index(temp.path());

    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    let missing = memory_path("/var/memory/self/missing.md");
    assert!(
        index
            .delete_backlog_row(&tenant, &missing, MemoryVersion(99))
            .is_ok()
    );
    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);
}

#[test]
fn bump_backlog_retry_on_missing_row_is_not_error() {
    let temp = tempfile::tempdir().unwrap();
    let tenant = tenant("tenant-a");
    let index = make_index(temp.path());

    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    let missing = memory_path("/var/memory/self/missing.md");
    assert!(
        index
            .bump_backlog_retry(&tenant, &missing, MemoryVersion(99), "embedder err")
            .is_ok()
    );
    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);
}
