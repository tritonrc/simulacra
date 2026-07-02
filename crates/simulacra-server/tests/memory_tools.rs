//! S037 Wave C — memory tool integration tests.
//!
//! These exercise `SemanticSearchTool` and `MemoryReadChunkTool` against
//! real `SqliteMemoryStore` + `SqliteVectorIndex` + `DefaultEmbedder`
//! instances over a tempdir, using the simulacra-tool `ToolRegistry` layer
//! just like the agent loop would.

use std::sync::Arc;

use serde_json::{Value, json};
use simulacra_memory::{
    DefaultEmbedder, Embedder, HitIdCache, IndexedChunk, MemoryStore, SqliteMemoryStore,
    SqliteVectorIndex, VectorIndex,
};
use simulacra_tool::{MemoryToolHandles, ToolRegistry, register_memory_tools};
use simulacra_types::{
    CapabilityToken, Locator, MemoryCapability, MemoryPath, MemoryVersion, TenantId,
};
use tempfile::TempDir;

struct Harness {
    _tmp: TempDir,
    tenant: TenantId,
    memory_store: Arc<dyn MemoryStore>,
    vector_index: Arc<dyn VectorIndex>,
    embedder: Arc<dyn Embedder>,
    hit_cache: Arc<HitIdCache>,
    capability: MemoryCapability,
}

fn build_harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let tenant = TenantId::parse("acme").unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(tmp.path()).unwrap());
    let embedder: Arc<dyn Embedder> = Arc::new(DefaultEmbedder::load_default().unwrap());
    let vector_index: Arc<dyn VectorIndex> =
        Arc::new(SqliteVectorIndex::new(tmp.path(), embedder.id().clone()).unwrap());
    let capability = MemoryCapability {
        enabled: true,
        search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
        write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
    };
    Harness {
        _tmp: tmp,
        tenant,
        memory_store,
        vector_index,
        embedder,
        hit_cache: Arc::new(HitIdCache::new()),
        capability,
    }
}

fn build_registry(h: &Harness) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_memory_tools(
        &mut registry,
        MemoryToolHandles {
            tenant: h.tenant.clone(),
            capability: h.capability.clone(),
            memory_store: Arc::clone(&h.memory_store),
            vector_index: Arc::clone(&h.vector_index),
            embedder: Arc::clone(&h.embedder),
            hit_cache: Arc::clone(&h.hit_cache),
            rrwb: None,
            hook_pipeline: None,
        },
    )
    .expect("memory tool registration should succeed");
    registry
}

fn capability_token(memory: MemoryCapability) -> CapabilityToken {
    CapabilityToken {
        memory,
        ..Default::default()
    }
}

/// Seed the memory store + vector index with a single chunk at `path`.
/// Returns the version written.
fn seed_chunk(h: &Harness, path_str: &str, text: &str) -> MemoryVersion {
    let path = MemoryPath::parse(path_str).unwrap();
    let version = h
        .memory_store
        .put(&h.tenant, &path, text.as_bytes())
        .unwrap();
    let embedding = h
        .embedder
        .embed(&[text])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let locator = Locator::Text {
        byte_start: 0,
        byte_end: text.len(),
    };
    let chunk = IndexedChunk {
        chunk_index: 0,
        locator,
        text: text.to_string(),
        embedding,
    };
    h.vector_index
        .upsert(&h.tenant, &path, version, h.embedder.id(), &[chunk])
        .unwrap();
    version
}

#[tokio::test]
async fn semantic_search_returns_empty_for_out_of_scope_without_error() {
    let h = build_harness();
    seed_chunk(
        &h,
        "/var/memory/self/note.md",
        "database schema information",
    );
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let result = registry
        .call(
            "semantic_search",
            json!({
                "query": "schema",
                "scope": "/var/memory/users",
            }),
            &token,
        )
        .await
        .expect("out-of-scope should not error");

    assert_eq!(result, json!({"hits": []}));
}

#[tokio::test]
async fn semantic_search_returns_matching_content_after_direct_upsert() {
    let h = build_harness();
    seed_chunk(
        &h,
        "/var/memory/self/note.md",
        "the quarterly close requires bigquery table X",
    );
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let result = registry
        .call(
            "semantic_search",
            json!({
                "query": "quarterly close bigquery",
                "scope": "/var/memory/self",
                "k": 5,
            }),
            &token,
        )
        .await
        .unwrap();

    let hits = result["hits"].as_array().expect("hits must be an array");
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(
        hits[0]["path"].as_str().unwrap(),
        "/var/memory/self/note.md"
    );
    assert!(hits[0]["hit_id"].as_str().unwrap().starts_with("hit_"));
}

#[tokio::test]
async fn memory_read_chunk_returns_content_for_valid_hit() {
    let h = build_harness();
    let text = "the quarterly close requires bigquery table X";
    seed_chunk(&h, "/var/memory/self/note.md", text);
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let search_result = registry
        .call(
            "semantic_search",
            json!({"query": "quarterly close", "scope": "/var/memory/self"}),
            &token,
        )
        .await
        .unwrap();
    let hit_id = search_result["hits"][0]["hit_id"]
        .as_str()
        .expect("hit id must exist");

    let chunk_result = registry
        .call("memory_read_chunk", json!({"hit_id": hit_id}), &token)
        .await
        .unwrap();

    assert_eq!(
        chunk_result["path"].as_str().unwrap(),
        "/var/memory/self/note.md"
    );
    assert_eq!(chunk_result["content"].as_str().unwrap(), text);
}

#[tokio::test]
async fn memory_read_chunk_returns_410_for_stale_version() {
    let h = build_harness();
    seed_chunk(&h, "/var/memory/self/note.md", "original content alpha");
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let search_result = registry
        .call(
            "semantic_search",
            json!({"query": "alpha", "scope": "/var/memory/self"}),
            &token,
        )
        .await
        .unwrap();
    let hit_id = search_result["hits"][0]["hit_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Bump version by writing new content and upserting a new chunk.
    seed_chunk(&h, "/var/memory/self/note.md", "new content bravo");

    let chunk_result = registry
        .call("memory_read_chunk", json!({"hit_id": hit_id}), &token)
        .await
        .unwrap();

    assert_eq!(chunk_result["error"], json!("chunk_stale"));
    assert_eq!(chunk_result["code"], json!(410));
}

#[tokio::test]
async fn memory_read_chunk_returns_410_for_deleted_path() {
    let h = build_harness();
    seed_chunk(&h, "/var/memory/self/note.md", "content to be deleted");
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let search_result = registry
        .call(
            "semantic_search",
            json!({"query": "deleted", "scope": "/var/memory/self"}),
            &token,
        )
        .await
        .unwrap();
    let hit_id = search_result["hits"][0]["hit_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Delete the path.
    let path = MemoryPath::parse("/var/memory/self/note.md").unwrap();
    h.memory_store.delete(&h.tenant, &path).unwrap();

    let chunk_result = registry
        .call("memory_read_chunk", json!({"hit_id": hit_id}), &token)
        .await
        .unwrap();

    assert_eq!(chunk_result["error"], json!("chunk_deleted"));
    assert_eq!(chunk_result["code"], json!(410));
}

#[tokio::test]
async fn memory_read_chunk_returns_404_for_unknown_hit_id() {
    let h = build_harness();
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let result = registry
        .call(
            "memory_read_chunk",
            json!({"hit_id": "hit_bogus_unknown"}),
            &token,
        )
        .await
        .unwrap();

    assert_eq!(result["error"], json!("hit_not_found"));
    assert_eq!(result["code"], json!(404));
}

#[tokio::test]
async fn semantic_search_is_opt_in() {
    let h = build_harness();
    let mut disabled = h.capability.clone();
    disabled.enabled = false;

    let mut registry = ToolRegistry::new();
    register_memory_tools(
        &mut registry,
        MemoryToolHandles {
            tenant: h.tenant.clone(),
            capability: disabled,
            memory_store: Arc::clone(&h.memory_store),
            vector_index: Arc::clone(&h.vector_index),
            embedder: Arc::clone(&h.embedder),
            hit_cache: Arc::clone(&h.hit_cache),
            rrwb: None,
            hook_pipeline: None,
        },
    )
    .expect("disabled memory registration should succeed as a no-op");

    let tool_names: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
    assert!(
        !tool_names.iter().any(|n| n == "semantic_search"),
        "semantic_search must not be registered when memory.enabled=false"
    );
    assert!(
        !tool_names.iter().any(|n| n == "memory_read_chunk"),
        "memory_read_chunk must not be registered when memory.enabled=false"
    );

    // Calling the tool returns an unknown-tool error — sanity check.
    let token = capability_token(MemoryCapability::default());
    let err = registry
        .call(
            "semantic_search",
            json!({"query": "x", "scope": "/var/memory/self"}),
            &token,
        )
        .await;
    assert!(err.is_err());
    let _ = Value::Null;
}

/// S037 §9.3: `k` must advertise and enforce a maximum of 20.
#[tokio::test]
async fn semantic_search_k_schema_advertises_maximum_of_20() {
    let h = build_harness();
    let registry = build_registry(&h);
    let def = registry
        .definitions()
        .into_iter()
        .find(|d| d.name == "semantic_search")
        .expect("semantic_search must be registered");
    let props = def
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("schema must have properties");
    let k_schema = props.get("k").expect("k property must be present");
    assert_eq!(
        k_schema.get("maximum").and_then(Value::as_u64),
        Some(20),
        "k.maximum must be 20"
    );
}

/// S037 §9.3: a request with `k` above the cap must return no more than 20 hits.
#[tokio::test]
async fn semantic_search_k_over_max_returns_at_most_20_hits() {
    let h = build_harness();
    for i in 0..25 {
        seed_chunk(
            &h,
            &format!("/var/memory/self/note-{i}.md"),
            &format!("quarterly close note {i} with bigquery schema details"),
        );
    }
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let result = registry
        .call(
            "semantic_search",
            json!({
                "query": "quarterly close bigquery",
                "scope": "/var/memory/self",
                "k": 100,
            }),
            &token,
        )
        .await
        .unwrap();

    let hits = result["hits"].as_array().expect("hits must be an array");
    assert!(
        hits.len() <= 20,
        "k clamp violated: got {} hits, expected <= 20",
        hits.len()
    );
}

/// S037 §9.3: `min_cosine` must advertise the valid range [-1.0, 1.0].
#[tokio::test]
async fn semantic_search_min_cosine_schema_advertises_unit_range() {
    let h = build_harness();
    let registry = build_registry(&h);
    let def = registry
        .definitions()
        .into_iter()
        .find(|d| d.name == "semantic_search")
        .expect("semantic_search must be registered");
    let props = def
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("schema must have properties");
    let mc_schema = props
        .get("min_cosine")
        .expect("min_cosine property must be present");
    assert_eq!(
        mc_schema.get("minimum").and_then(Value::as_f64),
        Some(-1.0),
        "min_cosine.minimum must be -1.0"
    );
    assert_eq!(
        mc_schema.get("maximum").and_then(Value::as_f64),
        Some(1.0),
        "min_cosine.maximum must be 1.0"
    );
}

/// S037 §9.3: a `min_cosine` above 1.0 is clamped to 1.0. Since cosine
/// similarity cannot exceed 1.0, the effective floor is unreachable and the
/// search returns no hits.
#[tokio::test]
async fn semantic_search_min_cosine_above_1_is_clamped_and_returns_empty() {
    let h = build_harness();
    seed_chunk(
        &h,
        "/var/memory/self/note.md",
        "quarterly close requires bigquery schema",
    );
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let result = registry
        .call(
            "semantic_search",
            json!({
                "query": "quarterly close bigquery",
                "scope": "/var/memory/self",
                "min_cosine": 100.0,
            }),
            &token,
        )
        .await
        .expect("out-of-range min_cosine must be clamped, not error");

    let hits = result["hits"].as_array().expect("hits must be an array");
    assert!(
        hits.is_empty(),
        "min_cosine clamped to 1.0 should admit no real hits"
    );
}

/// S037 §9.3: a `min_cosine` below -1.0 is clamped to -1.0, which admits
/// every hit — verifying the clamp does not cause errors and permits search.
#[tokio::test]
async fn semantic_search_min_cosine_below_neg1_is_clamped_and_permits_hits() {
    let h = build_harness();
    seed_chunk(
        &h,
        "/var/memory/self/note.md",
        "quarterly close requires bigquery schema",
    );
    let registry = build_registry(&h);
    let token = capability_token(h.capability.clone());

    let result = registry
        .call(
            "semantic_search",
            json!({
                "query": "quarterly close bigquery",
                "scope": "/var/memory/self",
                "min_cosine": -100.0,
            }),
            &token,
        )
        .await
        .expect("out-of-range min_cosine must be clamped, not error");

    let hits = result["hits"].as_array().expect("hits must be an array");
    assert!(
        !hits.is_empty(),
        "min_cosine clamped to -1.0 must permit matching hits"
    );
}
