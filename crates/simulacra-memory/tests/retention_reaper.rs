//! S037 §20 Retention assertions covered by this file:
//! - `[memory.retention]` is parsed and applied per subtree
//! - expired content is deleted from `MemoryStore` and the vector index
//! - the reaper runs at the configured interval (default 1h)
//! - tenant-local sweeps do not block other tenants
//! - paginated sweeps bound per-batch work and release the per-tenant lock between batches
//! - `memory_reaper_sweep` spans record sweep attributes (per S037 §18)

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simulacra_config::SimulacraConfig;
use simulacra_memory::{
    EmbedderId, IndexedChunk, MemoryError, MemoryStore, RetentionReaper, RetentionReaperConfig,
    RetentionSubtree as ReaperSubtree, SqliteMemoryStore, SqliteVectorIndex, UpsertOutcome,
    VectorIndex,
};
use simulacra_types::{Locator, MemoryPath, MemoryVersion, TenantId};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::SubscriberExt;

fn tenant(value: &str) -> TenantId {
    TenantId::parse(value).unwrap()
}

fn memory_path(value: &str) -> MemoryPath {
    MemoryPath::parse(value).unwrap()
}

fn test_embedder_id() -> EmbedderId {
    EmbedderId::new("retention-test-embedder", "1.0", 3)
}

fn reaper_subtree(prefix: &str, ttl: Duration) -> ReaperSubtree {
    ReaperSubtree {
        prefix: memory_path(prefix),
        ttl,
    }
}

fn parse_config(toml: &str) -> SimulacraConfig {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("simulacra.toml");
    fs::write(&config_path, toml).unwrap();
    SimulacraConfig::from_file(config_path.to_str().unwrap()).unwrap()
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

async fn age_past_ttl(ttl: Duration) {
    sleep(ttl + Duration::from_millis(75)).await;
}

fn assert_not_found(
    store: &SqliteMemoryStore,
    tenant: &TenantId,
    path: &MemoryPath,
) -> Result<(), MemoryError> {
    match store.get(tenant, path) {
        Err(MemoryError::NotFound(_)) => Ok(()),
        other => panic!("expected NotFound for {}, got {other:?}", path.as_str()),
    }
}

fn indexed_chunk(text: &str) -> IndexedChunk {
    IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text {
            byte_start: 0,
            byte_end: text.len(),
        },
        text: text.to_string(),
        embedding: vec![1.0, 0.0, 0.0],
    }
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span_ref) = ctx.span(id) {
            let span_name = span_ref.name().to_string();
            let mut new_fields = HashMap::new();
            let mut visitor = FieldVisitor(&mut new_fields);
            values.record(&mut visitor);

            let mut spans = self.spans.lock().unwrap();
            for captured in spans.iter_mut().rev() {
                if captured.name == span_name {
                    for (key, value) in new_fields.drain() {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

async fn capture_with_subscriber<F, Fut, T>(operation: F) -> (T, Vec<CapturedSpan>)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer {
        spans: Arc::clone(&spans),
    });

    let result = operation().with_subscriber(subscriber).await;
    let spans = spans.lock().unwrap().clone();
    (result, spans)
}

#[allow(dead_code)] // available for future per-sweep assertions; current test filters instead
fn span_named<'a>(spans: &'a [CapturedSpan], name: &str) -> &'a CapturedSpan {
    spans
        .iter()
        .rev()
        .find(|span| span.name == name)
        .unwrap_or_else(|| panic!("missing span {name}; spans={spans:?}"))
}

struct Harness {
    _temp: TempDir,
    store: Arc<SqliteMemoryStore>,
    index: Arc<SqliteVectorIndex>,
    embedder_id: EmbedderId,
}

impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let embedder_id = test_embedder_id();
        let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
        let index = Arc::new(SqliteVectorIndex::new(temp.path(), embedder_id.clone()).unwrap());
        Self {
            _temp: temp,
            store,
            index,
            embedder_id,
        }
    }

    fn reaper(
        &self,
        interval: Duration,
        batch_size: u64,
        subtrees: Vec<ReaperSubtree>,
    ) -> RetentionReaper {
        let store: Arc<dyn MemoryStore> = self.store.clone();
        let index: Arc<dyn VectorIndex> = self.index.clone();
        RetentionReaper::new(
            RetentionReaperConfig {
                interval,
                batch_size,
                subtrees,
            },
            store,
            index,
        )
    }

    fn seed_indexed_entry(
        &self,
        tenant: &TenantId,
        path: &MemoryPath,
        text: &str,
    ) -> MemoryVersion {
        let version = self.store.put(tenant, path, text.as_bytes()).unwrap();
        let outcome = self
            .index
            .upsert(
                tenant,
                path,
                version,
                &self.embedder_id,
                &[indexed_chunk(text)],
            )
            .unwrap();
        assert_eq!(outcome, UpsertOutcome::Applied);
        version
    }
}

async fn wait_for_partial_progress(
    store: &SqliteMemoryStore,
    tenant: &TenantId,
    prefix: &MemoryPath,
    total_entries: usize,
) {
    wait_until("partial retention progress", || {
        let remaining = store.list_prefix(tenant, prefix).unwrap().len();
        remaining > 0 && remaining < total_entries
    })
    .await;
}

// --- Config parsing ---

#[test]
fn retention_config_parses_subtree_ttls_from_toml() {
    let config = parse_config(
        r#"
        [project]
        name = "simulacra"

        [agent_types.default]
        model = "claude-sonnet-4.6"

        [memory]
        dir = "/tmp/simulacra-memory"

        [memory.retention]
        interval_secs = 120
        batch_size = 17

        [[memory.retention.subtrees]]
        prefix = "/var/memory/ephemeral"
        ttl_secs = 30

        [[memory.retention.subtrees]]
        prefix = "/mnt/transient"
        ttl_secs = 90
        "#,
    );

    let retention = config.memory.unwrap().retention.unwrap();
    assert_eq!(retention.interval_secs, 120);
    assert_eq!(retention.batch_size, 17);
    assert_eq!(retention.subtrees.len(), 2);
    assert_eq!(retention.subtrees[0].prefix, "/var/memory/ephemeral");
    assert_eq!(retention.subtrees[0].ttl_secs, 30);
    assert_eq!(retention.subtrees[1].prefix, "/mnt/transient");
    assert_eq!(retention.subtrees[1].ttl_secs, 90);

    let applied = RetentionReaperConfig {
        interval: Duration::from_secs(retention.interval_secs),
        batch_size: retention.batch_size,
        subtrees: retention
            .subtrees
            .iter()
            .map(|subtree| ReaperSubtree {
                prefix: memory_path(&subtree.prefix),
                ttl: Duration::from_secs(subtree.ttl_secs),
            })
            .collect(),
    };

    assert_eq!(applied.interval, Duration::from_secs(120));
    assert_eq!(applied.batch_size, 17);
    assert_eq!(
        applied.subtrees[0].prefix,
        memory_path("/var/memory/ephemeral")
    );
    assert_eq!(applied.subtrees[0].ttl, Duration::from_secs(30));
    assert_eq!(applied.subtrees[1].prefix, memory_path("/mnt/transient"));
    assert_eq!(applied.subtrees[1].ttl, Duration::from_secs(90));
}

#[test]
fn retention_config_defaults_interval_to_3600_when_missing() {
    let config = parse_config(
        r#"
        [project]
        name = "simulacra"

        [agent_types.default]
        model = "claude-sonnet-4.6"

        [memory]
        dir = "/tmp/simulacra-memory"

        [memory.retention]

        [[memory.retention.subtrees]]
        prefix = "/var/memory/ephemeral"
        ttl_secs = 7
        "#,
    );

    let retention = config.memory.unwrap().retention.unwrap();
    assert_eq!(retention.interval_secs, 3600);
    assert_eq!(retention.batch_size, 256);
    assert_eq!(retention.subtrees.len(), 1);
    assert_eq!(retention.subtrees[0].prefix, "/var/memory/ephemeral");
    assert_eq!(retention.subtrees[0].ttl_secs, 7);
}

// --- Sweep behavior ---

#[tokio::test(flavor = "multi_thread")]
async fn sweep_deletes_expired_entries_from_memory_store() {
    let harness = Harness::new();
    let tenant = tenant("tenant-a");
    let ttl = Duration::from_millis(50);
    let paths = [
        memory_path("/var/memory/ephemeral/one.txt"),
        memory_path("/var/memory/ephemeral/two.txt"),
        memory_path("/var/memory/ephemeral/three.txt"),
    ];

    for (idx, path) in paths.iter().enumerate() {
        harness.seed_indexed_entry(&tenant, path, &format!("expired-{idx}"));
    }
    age_past_ttl(ttl).await;

    let reaper = harness.reaper(
        Duration::from_secs(3600),
        256,
        vec![reaper_subtree("/var/memory/ephemeral", ttl)],
    );
    let stats = reaper.sweep_now(&tenant).await.unwrap();

    assert_eq!(stats.paths_deleted, paths.len() as u64);
    assert_eq!(stats.subtrees_scanned, 1);
    for path in &paths {
        assert_not_found(harness.store.as_ref(), &tenant, path).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_deletes_expired_entries_from_vector_index() {
    let harness = Harness::new();
    let tenant = tenant("tenant-a");
    let ttl = Duration::from_millis(50);
    let seeded = [
        (
            memory_path("/var/memory/ephemeral/index-one.txt"),
            "indexed-expired-one",
        ),
        (
            memory_path("/var/memory/ephemeral/index-two.txt"),
            "indexed-expired-two",
        ),
        (
            memory_path("/var/memory/ephemeral/index-three.txt"),
            "indexed-expired-three",
        ),
    ];
    let mut versions = Vec::new();
    for (path, text) in &seeded {
        versions.push((
            path.clone(),
            harness.seed_indexed_entry(&tenant, path, text),
        ));
    }
    age_past_ttl(ttl).await;

    let reaper = harness.reaper(
        Duration::from_secs(3600),
        256,
        vec![reaper_subtree("/var/memory/ephemeral", ttl)],
    );
    let stats = reaper.sweep_now(&tenant).await.unwrap();

    assert_eq!(stats.paths_deleted, seeded.len() as u64);
    for (path, version) in versions {
        assert!(
            harness
                .index
                .get_chunk(&tenant, &path, version, 0)
                .unwrap()
                .is_none(),
            "expected index rows for {} to be gone",
            path.as_str()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_preserves_non_expired_entries_in_same_subtree() {
    let harness = Harness::new();
    let tenant = tenant("tenant-a");
    let ttl = Duration::from_millis(60);
    let expired_paths = [
        memory_path("/var/memory/ephemeral/expired-a.txt"),
        memory_path("/var/memory/ephemeral/expired-b.txt"),
    ];
    let fresh_paths = [
        memory_path("/var/memory/ephemeral/fresh-a.txt"),
        memory_path("/var/memory/ephemeral/fresh-b.txt"),
    ];

    harness.seed_indexed_entry(&tenant, &expired_paths[0], "expired-a");
    harness.seed_indexed_entry(&tenant, &expired_paths[1], "expired-b");
    age_past_ttl(ttl).await;
    harness.seed_indexed_entry(&tenant, &fresh_paths[0], "fresh-a");
    harness.seed_indexed_entry(&tenant, &fresh_paths[1], "fresh-b");

    let reaper = harness.reaper(
        Duration::from_secs(3600),
        256,
        vec![reaper_subtree("/var/memory/ephemeral", ttl)],
    );
    let stats = reaper.sweep_now(&tenant).await.unwrap();

    assert_eq!(stats.paths_deleted, 2);
    for path in &expired_paths {
        assert_not_found(harness.store.as_ref(), &tenant, path).unwrap();
    }
    assert_eq!(
        harness.store.get(&tenant, &fresh_paths[0]).unwrap().0,
        b"fresh-a".to_vec()
    );
    assert_eq!(
        harness.store.get(&tenant, &fresh_paths[1]).unwrap().0,
        b"fresh-b".to_vec()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_ignores_entries_outside_configured_subtrees() {
    let harness = Harness::new();
    let tenant = tenant("tenant-a");
    let ttl = Duration::from_millis(50);
    let expired = memory_path("/var/memory/ephemeral/delete-me.txt");
    let outside = memory_path("/var/memory/self/keep-me.txt");

    harness.seed_indexed_entry(&tenant, &expired, "ephemeral");
    harness.seed_indexed_entry(&tenant, &outside, "self");
    age_past_ttl(ttl).await;

    let reaper = harness.reaper(
        Duration::from_secs(3600),
        256,
        vec![reaper_subtree("/var/memory/ephemeral", ttl)],
    );
    let stats = reaper.sweep_now(&tenant).await.unwrap();

    assert_eq!(stats.paths_deleted, 1);
    assert_not_found(harness.store.as_ref(), &tenant, &expired).unwrap();
    assert_eq!(
        harness.store.get(&tenant, &outside).unwrap().0,
        b"self".to_vec()
    );
}

// --- Pagination ---

#[tokio::test(flavor = "multi_thread")]
async fn sweep_paginates_batches_of_configured_size() {
    let harness = Harness::new();
    let tenant = tenant("tenant-a");
    let prefix = memory_path("/var/memory/ephemeral");
    let writer_path = memory_path("/var/memory/ephemeral/interleaved-write.txt");
    let ttl = Duration::from_millis(50);
    let expired_count = 64usize;

    for idx in 0..expired_count {
        let path = memory_path(&format!("/var/memory/ephemeral/item-{idx}.txt"));
        harness.seed_indexed_entry(&tenant, &path, &format!("expired-{idx}"));
    }
    age_past_ttl(ttl).await;

    let reaper = Arc::new(harness.reaper(
        Duration::from_secs(3600),
        1,
        vec![reaper_subtree("/var/memory/ephemeral", ttl)],
    ));
    let reaper_for_task = Arc::clone(&reaper);
    let tenant_for_task = tenant.clone();
    let sweep = tokio::spawn(async move { reaper_for_task.sweep_now(&tenant_for_task).await });

    wait_for_partial_progress(harness.store.as_ref(), &tenant, &prefix, expired_count).await;

    timeout(Duration::from_secs(1), async {
        harness
            .store
            .put(&tenant, &writer_path, b"writer survived between batches")
            .unwrap();
    })
    .await
    .expect("parallel write should succeed while the sweep is still paginating");

    let stats = timeout(Duration::from_secs(3), sweep)
        .await
        .expect("paginated sweep should finish")
        .unwrap()
        .unwrap();

    assert_eq!(stats.paths_deleted, expired_count as u64);
    assert!(stats.batches > 1, "expected more than one batch");
    assert_eq!(
        harness.store.get(&tenant, &writer_path).unwrap().0,
        b"writer survived between batches".to_vec()
    );
}

// --- Tenant isolation ---

#[tokio::test(flavor = "multi_thread")]
async fn sweep_is_tenant_isolated_so_tenant_a_does_not_block_tenant_b() {
    let harness = Harness::new();
    let tenant_a = tenant("tenant-a");
    let tenant_b = tenant("tenant-b");
    let ttl = Duration::from_millis(50);
    let path_b = memory_path("/var/memory/ephemeral/b-only.txt");
    let a_count = 64usize;

    for idx in 0..a_count {
        let path = memory_path(&format!("/var/memory/ephemeral/a-{idx}.txt"));
        harness.seed_indexed_entry(&tenant_a, &path, &format!("tenant-a-{idx}"));
    }
    harness.seed_indexed_entry(&tenant_b, &path_b, "tenant-b");
    age_past_ttl(ttl).await;

    let reaper = Arc::new(harness.reaper(
        Duration::from_secs(3600),
        1,
        vec![reaper_subtree("/var/memory/ephemeral", ttl)],
    ));
    reaper.register_tenant(tenant_a.clone());
    reaper.register_tenant(tenant_b.clone());

    let reaper_for_a = Arc::clone(&reaper);
    let tenant_a_for_task = tenant_a.clone();
    let sweep_a = tokio::spawn(async move { reaper_for_a.sweep_now(&tenant_a_for_task).await });

    let reaper_for_b = Arc::clone(&reaper);
    let tenant_b_for_task = tenant_b.clone();
    let sweep_b = tokio::spawn(async move { reaper_for_b.sweep_now(&tenant_b_for_task).await });

    let stats_b = timeout(Duration::from_secs(1), sweep_b)
        .await
        .expect("tenant B sweep should complete promptly")
        .unwrap()
        .unwrap();
    assert_eq!(stats_b.paths_deleted, 1);
    assert!(
        !sweep_a.is_finished(),
        "tenant A sweep should still be running when tenant B finishes"
    );
    assert_not_found(harness.store.as_ref(), &tenant_b, &path_b).unwrap();

    let stats_a = timeout(Duration::from_secs(3), sweep_a)
        .await
        .expect("tenant A sweep should eventually complete")
        .unwrap()
        .unwrap();
    assert_eq!(stats_a.paths_deleted, a_count as u64);
}

// --- Background loop ---

#[tokio::test(flavor = "multi_thread")]
async fn reaper_background_loop_runs_sweep_at_interval() {
    let harness = Harness::new();
    let tenant = tenant("tenant-a");
    let path = memory_path("/var/memory/ephemeral/background.txt");
    let ttl = Duration::from_millis(75);

    harness.seed_indexed_entry(&tenant, &path, "background sweep target");
    age_past_ttl(ttl).await;

    let reaper = harness.reaper(
        Duration::from_millis(200),
        256,
        vec![reaper_subtree("/var/memory/ephemeral", ttl)],
    );
    reaper.register_tenant(tenant.clone());

    wait_until("background retention sweep", || {
        matches!(
            harness.store.get(&tenant, &path),
            Err(MemoryError::NotFound(_))
        )
    })
    .await;

    reaper.shutdown().await.unwrap();
}

// --- Observability ---

// Per S037 §18: each per-subtree sweep emits one `memory_reaper_sweep` span
// with `tenant`, `subtree`, `deleted_count`, `duration_ms` attributes — not
// one tenant-wide span. This test exercises both the span name and the
// per-subtree granularity (two subtrees → two spans).
#[tokio::test(flavor = "multi_thread")]
async fn sweep_emits_memory_reaper_sweep_span_per_subtree() {
    let harness = Harness::new();
    let tenant = tenant("tenant-a");
    let ttl = Duration::from_millis(50);
    let ephemeral_paths = [
        memory_path("/var/memory/ephemeral/observed-a.txt"),
        memory_path("/var/memory/ephemeral/observed-b.txt"),
    ];
    let transient_paths = [memory_path("/mnt/transient/observed-c.txt")];

    for (idx, path) in ephemeral_paths.iter().enumerate() {
        harness.seed_indexed_entry(&tenant, path, &format!("eph-{idx}"));
    }
    for (idx, path) in transient_paths.iter().enumerate() {
        harness.seed_indexed_entry(&tenant, path, &format!("tr-{idx}"));
    }
    age_past_ttl(ttl).await;

    let reaper = harness.reaper(
        Duration::from_secs(3600),
        256,
        vec![
            reaper_subtree("/var/memory/ephemeral", ttl),
            reaper_subtree("/mnt/transient", ttl),
        ],
    );
    let (stats, spans) =
        capture_with_subscriber(|| async { reaper.sweep_now(&tenant).await.unwrap() }).await;

    assert_eq!(
        stats.paths_deleted,
        (ephemeral_paths.len() + transient_paths.len()) as u64
    );

    // One span per subtree — spec §18 schema, not tenant-aggregate.
    let sweep_spans: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "memory_reaper_sweep")
        .collect();
    assert_eq!(
        sweep_spans.len(),
        2,
        "expected one span per subtree, got {sweep_spans:?}"
    );

    for span in &sweep_spans {
        assert_eq!(
            span.fields.get("tenant").map(String::as_str),
            Some(tenant.as_str())
        );
        assert!(
            span.fields.contains_key("subtree"),
            "{span:?} missing `subtree` attribute"
        );
        assert!(
            span.fields.contains_key("deleted_count"),
            "{span:?} missing `deleted_count` attribute"
        );
        assert!(
            span.fields.contains_key("duration_ms"),
            "{span:?} missing `duration_ms` attribute"
        );
    }
}

// --- Lifecycle ---

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_drains_pending_sweeps_and_returns_within_bound() {
    let harness = Harness::new();
    let tenant = tenant("tenant-a");
    let prefix = memory_path("/var/memory/ephemeral");
    let ttl = Duration::from_millis(50);
    let expired_count = 64usize;

    for idx in 0..expired_count {
        let path = memory_path(&format!("/var/memory/ephemeral/shutdown-{idx}.txt"));
        harness.seed_indexed_entry(&tenant, &path, &format!("shutdown-{idx}"));
    }
    age_past_ttl(ttl).await;

    let reaper = harness.reaper(
        Duration::from_millis(25),
        1,
        vec![reaper_subtree("/var/memory/ephemeral", ttl)],
    );
    reaper.register_tenant(tenant.clone());

    wait_for_partial_progress(harness.store.as_ref(), &tenant, &prefix, expired_count).await;

    timeout(Duration::from_secs(1), reaper.shutdown())
        .await
        .expect("shutdown should return within the bound")
        .unwrap();

    assert!(
        harness
            .store
            .list_prefix(&tenant, &prefix)
            .unwrap()
            .is_empty(),
        "shutdown should drain the in-flight sweep before returning"
    );
}
