// Tests hold a Mutex guard across await points intentionally to serialize
// concurrent test runs that share global OTel state.
#![allow(clippy::await_holding_lock)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use rusqlite::{Connection, params};
use simulacra_memory::{
    BackgroundEmbedder, BackgroundEmbedderConfig, Chunk, Chunker, ChunkerSelector, DefaultEmbedder,
    Embedder, MemoryEvent, MemoryRecvOutcome, MemoryStore, SqliteMemoryStore, SqliteVectorIndex,
    VectorIndex,
};
use simulacra_types::{Locator, MemoryPath, MemoryVersion, TenantId};
use tempfile::TempDir;
use tokio::time::sleep;

const EMBED_LAG_METRIC: &str = "simulacra_memory_embed_lag_seconds";
const QUEUE_DEPTH_METRIC: &str = "simulacra_memory_queue_depth";
const REINDEX_BACKLOG_METRIC: &str = "simulacra_memory_reindex_backlog";

#[derive(Clone, Debug, PartialEq)]
struct MetricPoint {
    metric: String,
    attributes: BTreeMap<String, String>,
    value: MetricValue,
}

#[derive(Clone, Debug, PartialEq)]
enum MetricValue {
    GaugeU64(u64),
    SumU64(u64),
    HistogramF64 {
        count: u64,
        sum: f64,
        bounds: Vec<f64>,
        bucket_counts: Vec<u64>,
    },
}

#[derive(Clone, Debug, Default)]
struct TestMetricExporter {
    latest: Arc<Mutex<Vec<MetricPoint>>>,
}

impl TestMetricExporter {
    fn latest_points(&self) -> Vec<MetricPoint> {
        self.latest.lock().unwrap().clone()
    }

    fn reset(&self) {
        self.latest.lock().unwrap().clear();
    }
}

impl PushMetricExporter for TestMetricExporter {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let mut snapshot = Vec::new();

        for scope_metrics in metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                        for point in gauge.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                attributes: attrs(point.attributes()),
                                value: MetricValue::GaugeU64(point.value()),
                            });
                        }
                    }
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                        for point in sum.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                attributes: attrs(point.attributes()),
                                value: MetricValue::SumU64(point.value()),
                            });
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                        for point in histogram.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                attributes: attrs(point.attributes()),
                                value: MetricValue::HistogramF64 {
                                    count: point.count(),
                                    sum: point.sum(),
                                    bounds: point.bounds().collect(),
                                    bucket_counts: point.bucket_counts().collect(),
                                },
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        *self.latest.lock().unwrap() = snapshot;
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        Temporality::Cumulative
    }
}

#[derive(Debug)]
struct TestTelemetry {
    meter_provider: SdkMeterProvider,
    metric_exporter: TestMetricExporter,
}

impl TestTelemetry {
    fn install() -> &'static Self {
        static TELEMETRY: OnceLock<TestTelemetry> = OnceLock::new();

        TELEMETRY.get_or_init(|| {
            let metric_exporter = TestMetricExporter::default();
            let reader = PeriodicReader::builder(metric_exporter.clone())
                .with_interval(Duration::from_millis(10))
                .build();
            let meter_provider = SdkMeterProvider::builder().with_reader(reader).build();
            global::set_meter_provider(meter_provider.clone());

            TestTelemetry {
                meter_provider,
                metric_exporter,
            }
        })
    }

    fn reset(&self) {
        self.metric_exporter.reset();
    }

    fn flush_metrics(&self) -> Vec<MetricPoint> {
        self.meter_provider.force_flush().unwrap();
        self.metric_exporter.latest_points()
    }
}

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn attrs<'a>(values: impl Iterator<Item = &'a KeyValue>) -> BTreeMap<String, String> {
    values
        .map(|kv| (kv.key.as_str().to_string(), kv.value.as_str().into_owned()))
        .collect()
}

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

struct Harness {
    temp: TempDir,
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
                enqueue_timeout: Duration::from_millis(10),
                embed_batch_size: 1,
            },
        )
        .unwrap()
    }

    fn vector_index(&self) -> &dyn VectorIndex {
        self.index.as_ref()
    }
}

fn always_select(chunker: Arc<dyn Chunker>) -> ChunkerSelector {
    Arc::new(move |_| Some(chunker.clone()))
}

fn tenant_db_path(root: &Path, tenant: &TenantId) -> PathBuf {
    root.join("memory")
        .join(format!("{}.db", tenant.as_fs_segment()))
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

async fn wait_until(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if predicate() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        sleep(Duration::from_millis(25)).await;
    }
}

fn gauge_u64(points: &[MetricPoint], metric: &str, tenant: &TenantId) -> Option<u64> {
    points.iter().find_map(|point| {
        if point.metric != metric {
            return None;
        }
        if point.attributes.get("tenant").map(String::as_str) != Some(tenant.as_str()) {
            return None;
        }
        match point.value {
            MetricValue::GaugeU64(value) => Some(value),
            _ => None,
        }
    })
}

#[derive(Clone, Debug, PartialEq)]
struct HistogramSnapshot {
    count: u64,
    sum: f64,
    bounds: Vec<f64>,
    bucket_counts: Vec<u64>,
}

fn histogram_f64(
    points: &[MetricPoint],
    metric: &str,
    tenant: &TenantId,
) -> Option<HistogramSnapshot> {
    points.iter().find_map(|point| {
        if point.metric != metric {
            return None;
        }
        if point.attributes.get("tenant").map(String::as_str) != Some(tenant.as_str()) {
            return None;
        }
        match &point.value {
            MetricValue::HistogramF64 {
                count,
                sum,
                bounds,
                bucket_counts,
            } => Some(HistogramSnapshot {
                count: *count,
                sum: *sum,
                bounds: bounds.clone(),
                bucket_counts: bucket_counts.clone(),
            }),
            _ => None,
        }
    })
}

fn strictly_increasing(values: &[f64]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

async fn burst_writes(store: Arc<SqliteMemoryStore>, tenant: TenantId, prefix: &str, count: usize) {
    let payload = "semantic token ".repeat(4_096);
    for i in 0..count {
        let path = memory_path(&format!("/var/memory/self/{prefix}-{i}.md"));
        store.put(&tenant, &path, payload.as_bytes()).unwrap();
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn embed_lag_histogram_is_emitted_on_put_event() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    // Dedicated tenant so the cumulative histogram count is not
    // polluted by other tests' tenants in the same process.
    let tenant = tenant("tenant-embed-lag-put");
    let path = memory_path("/var/memory/self/lag.md");
    let mut receiver = harness.store.subscribe().unwrap();
    let before_put = SystemTime::now();
    let version = harness
        .store
        .put(&tenant, &path, b"alpha semantic token")
        .unwrap();

    let produced_at = match receiver.recv().await {
        MemoryRecvOutcome::Event(MemoryEvent::Put {
            tenant: observed_tenant,
            path: observed_path,
            version: observed_version,
            produced_at,
            ..
        }) => {
            assert_eq!(observed_tenant, tenant);
            assert_eq!(observed_path, path);
            assert_eq!(observed_version, version);
            produced_at
        }
        other => panic!("expected put event, got {other:?}"),
    };
    assert!(produced_at >= before_put);
    assert!(produced_at <= SystemTime::now());

    wait_until("chunk row for embed lag metric", || {
        chunk_rows(harness.temp.path(), &tenant, &path)
            == vec![(0, version, "alpha semantic token".to_string())]
    })
    .await;

    wait_until("embed lag histogram point", || {
        histogram_f64(&telemetry.flush_metrics(), EMBED_LAG_METRIC, &tenant)
            .map(|histogram| histogram.count >= 1)
            .unwrap_or(false)
    })
    .await;

    let histogram = histogram_f64(&telemetry.flush_metrics(), EMBED_LAG_METRIC, &tenant).unwrap();
    assert_eq!(histogram.count, 1);
    assert!(histogram.sum >= 0.0);
}

#[tokio::test]
async fn embed_lag_histogram_has_percentile_buckets() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    // Dedicated tenant so the cumulative histogram count is not
    // polluted by other tests' tenants in the same process.
    let tenant = tenant("tenant-embed-lag-buckets");
    let path = memory_path("/var/memory/self/percentiles.md");
    let version = harness
        .store
        .put(&tenant, &path, b"bucket coverage token")
        .unwrap();

    wait_until("chunk row for percentile buckets", || {
        chunk_rows(harness.temp.path(), &tenant, &path)
            == vec![(0, version, "bucket coverage token".to_string())]
    })
    .await;

    wait_until("embed lag histogram with buckets", || {
        histogram_f64(&telemetry.flush_metrics(), EMBED_LAG_METRIC, &tenant).is_some()
    })
    .await;

    let histogram = histogram_f64(&telemetry.flush_metrics(), EMBED_LAG_METRIC, &tenant).unwrap();
    assert!(
        histogram.bounds.len() >= 3,
        "expected percentile-friendly p50/p95/p99 buckets"
    );
    assert!(strictly_increasing(&histogram.bounds));
    assert_eq!(histogram.bucket_counts.len(), histogram.bounds.len() + 1);
    assert_eq!(
        histogram.bucket_counts.iter().sum::<u64>(),
        histogram.count,
        "histogram bucket counts should sum to the sample count"
    );
}

#[tokio::test]
async fn queue_depth_gauge_reports_per_tenant() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let tenant_a = tenant("tenant-a");
    let tenant_b = tenant("tenant-b");

    let writer_a = tokio::spawn(burst_writes(
        harness.store.clone(),
        tenant_a.clone(),
        "queue-a",
        64,
    ));
    let writer_b = tokio::spawn(burst_writes(
        harness.store.clone(),
        tenant_b.clone(),
        "queue-b",
        64,
    ));

    // The gauge must report at least one point per tenant with a
    // `tenant` attribute matching each tenant id. Depth values are
    // timing-sensitive; the per-tenant labeling is the invariant this
    // test verifies.
    wait_until("per-tenant queue depth gauge", || {
        let points = telemetry.flush_metrics();
        gauge_u64(&points, QUEUE_DEPTH_METRIC, &tenant_a).is_some()
            && gauge_u64(&points, QUEUE_DEPTH_METRIC, &tenant_b).is_some()
    })
    .await;

    writer_a.await.unwrap();
    writer_b.await.unwrap();
}

#[tokio::test]
async fn queue_depth_gauge_zero_when_empty() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let tenant = tenant("tenant-c");
    let path = memory_path("/var/memory/self/queue-empty.md");
    let version = harness.store.put(&tenant, &path, b"drain queue").unwrap();

    wait_until("queue drain indexing", || {
        chunk_rows(harness.temp.path(), &tenant, &path)
            == vec![(0, version, "drain queue".to_string())]
    })
    .await;

    wait_until("queue depth zero gauge", || {
        gauge_u64(&telemetry.flush_metrics(), QUEUE_DEPTH_METRIC, &tenant) == Some(0)
    })
    .await;
}

#[tokio::test]
async fn reindex_backlog_gauge_reports_per_tenant() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let index = harness.vector_index();
    let tenant_a = tenant("tenant-a");
    let tenant_b = tenant("tenant-b");

    index
        .enqueue_backlog_for(
            &tenant_a,
            &memory_path("/var/memory/self/backlog-a-1.md"),
            MemoryVersion(1),
        )
        .unwrap();
    index
        .enqueue_backlog_for(
            &tenant_a,
            &memory_path("/var/memory/self/backlog-a-2.md"),
            MemoryVersion(2),
        )
        .unwrap();
    index
        .enqueue_backlog_for(
            &tenant_b,
            &memory_path("/var/memory/self/backlog-b-1.md"),
            MemoryVersion(7),
        )
        .unwrap();

    assert_eq!(index.backlog_count(&tenant_a).unwrap(), 2);
    assert_eq!(index.backlog_count(&tenant_b).unwrap(), 1);

    wait_until("reindex backlog gauge per tenant", || {
        let points = telemetry.flush_metrics();
        gauge_u64(&points, REINDEX_BACKLOG_METRIC, &tenant_a) == Some(2)
            && gauge_u64(&points, REINDEX_BACKLOG_METRIC, &tenant_b) == Some(1)
    })
    .await;
}

#[tokio::test]
async fn reindex_backlog_gauge_is_zero_when_table_empty() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let harness = Harness::new();
    let chunker = Arc::new(MockChunker::new());
    let _background = harness.spawn(always_select(chunker));
    let index = harness.vector_index();
    let tenant = tenant("tenant-empty");

    index.ensure_tenant(&tenant).unwrap();
    assert_eq!(index.backlog_count(&tenant).unwrap(), 0);

    wait_until("reindex backlog zero gauge", || {
        gauge_u64(&telemetry.flush_metrics(), REINDEX_BACKLOG_METRIC, &tenant) == Some(0)
    })
    .await;
}

const EMBEDDER_LOAD_FAILURES_METRIC: &str = "simulacra_memory_embedder_load_failures_total";

fn counter_u64(points: &[MetricPoint], metric: &str, reason: &str) -> Option<u64> {
    points.iter().find_map(|p| {
        if p.metric != metric {
            return None;
        }
        if p.attributes.get("reason").map(String::as_str) != Some(reason) {
            return None;
        }
        if let MetricValue::SumU64(value) = p.value {
            Some(value)
        } else {
            None
        }
    })
}

/// S037 §20 Observability: `simulacra_memory_embedder_load_failures_total` is a
/// Counter that increments whenever the embedder cannot be loaded. The
/// `reason` attribute is low-cardinality so dashboards can fan out by
/// failure kind without blowing up series count.
#[tokio::test]
async fn embedder_load_failure_counter_increments_by_reason() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    simulacra_memory::record_embedder_load_failure("load_default");
    simulacra_memory::record_embedder_load_failure("load_default");
    simulacra_memory::record_embedder_load_failure("dim_mismatch");

    wait_until("embedder load failure counter reflects 2 + 1 adds", || {
        let points = telemetry.flush_metrics();
        let load_default = counter_u64(&points, EMBEDDER_LOAD_FAILURES_METRIC, "load_default");
        let dim_mismatch = counter_u64(&points, EMBEDDER_LOAD_FAILURES_METRIC, "dim_mismatch");
        load_default.unwrap_or(0) >= 2 && dim_mismatch.unwrap_or(0) >= 1
    })
    .await;

    let points = telemetry.flush_metrics();
    assert!(
        counter_u64(&points, EMBEDDER_LOAD_FAILURES_METRIC, "load_default").unwrap_or(0) >= 2,
        "load_default bucket must reflect 2 increments"
    );
    assert!(
        counter_u64(&points, EMBEDDER_LOAD_FAILURES_METRIC, "dim_mismatch").unwrap_or(0) >= 1,
        "dim_mismatch bucket must reflect 1 increment"
    );
}

const OVERFLOW_METRIC: &str = "simulacra_memory_overflow_total";

fn counter_u64_by(
    points: &[MetricPoint],
    metric: &str,
    attr_key: &str,
    attr_val: &str,
) -> Option<u64> {
    points.iter().find_map(|p| {
        if p.metric != metric {
            return None;
        }
        if p.attributes.get(attr_key).map(String::as_str) != Some(attr_val) {
            return None;
        }
        if let MetricValue::SumU64(value) = p.value {
            Some(value)
        } else {
            None
        }
    })
}

/// A chunker that blocks inside `chunk()` until `hold` is released, so
/// tests can deterministically saturate the per-tenant queue.
struct BlockingChunker {
    hold: Arc<std::sync::atomic::AtomicBool>,
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
            std::thread::sleep(Duration::from_millis(5));
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

/// RAII guard ensuring the BlockingChunker's hold is released even on
/// panic, so `BackgroundEmbedder::shutdown` never wedges waiting for a
/// blocked chunker.
struct ReleaseOnDrop(Arc<std::sync::atomic::AtomicBool>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// S037 §20 (R010 follow-up): `simulacra_memory_overflow_total` must
/// increment whenever a Put or Delete falls through the per-tenant
/// queue's overflow path, keyed on `kind`. This is the operator's
/// earliest signal that embedder capacity is undersized — the backlog
/// gauge only shows depth, not rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn overflow_counter_increments_by_kind_under_saturation() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let temp = tempfile::tempdir().unwrap();
    let embedder = Arc::new(DefaultEmbedder::load_default().unwrap());
    let store = Arc::new(SqliteMemoryStore::new(temp.path()).unwrap());
    let index = Arc::new(SqliteVectorIndex::new(temp.path(), embedder.id().clone()).unwrap());

    // Seed one path via direct upsert so a later Delete has something
    // concrete to target. Done before spawning the embedder so the
    // index write doesn't race with the blocked worker.
    let tenant = tenant("tenant-overflow-counter");
    let delete_target = memory_path("/var/memory/self/overflow-del-target.md");
    let pre_seed_version = store
        .put(&tenant, &delete_target, b"pre-seed for delete")
        .unwrap();
    let _ = pre_seed_version;

    let hold = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let _release = ReleaseOnDrop(Arc::clone(&hold));
    let chunker: Arc<dyn Chunker> = Arc::new(BlockingChunker {
        hold: Arc::clone(&hold),
    });

    let background = BackgroundEmbedder::spawn(
        store.clone(),
        index.clone(),
        embedder.clone(),
        always_select(chunker),
        BackgroundEmbedderConfig {
            queue_capacity: 1,
            enqueue_timeout: Duration::from_millis(50),
            embed_batch_size: 1,
        },
    )
    .unwrap();

    // Saturate the queue: first put enters the worker (blocks on chunker),
    // second fills the capacity-1 queue, third-and-beyond overflow.
    let put_a = memory_path("/var/memory/self/overflow-put-a.md");
    let put_b = memory_path("/var/memory/self/overflow-put-b.md");
    let put_c = memory_path("/var/memory/self/overflow-put-c.md");
    store.put(&tenant, &put_a, b"first").unwrap();
    sleep(Duration::from_millis(80)).await;
    store.put(&tenant, &put_b, b"second").unwrap();
    store.put(&tenant, &put_c, b"third overflow").unwrap();
    // Delete on the pre-seeded path: queue still saturated, so this
    // overflows as a Delete event.
    store.delete(&tenant, &delete_target).unwrap();

    wait_until("overflow counter reflects put and delete overflows", || {
        let points = telemetry.flush_metrics();
        let puts = counter_u64_by(&points, OVERFLOW_METRIC, "kind", "put").unwrap_or(0);
        let deletes = counter_u64_by(&points, OVERFLOW_METRIC, "kind", "delete").unwrap_or(0);
        puts >= 1 && deletes >= 1
    })
    .await;

    let points = telemetry.flush_metrics();
    assert!(
        counter_u64_by(&points, OVERFLOW_METRIC, "kind", "put").unwrap_or(0) >= 1,
        "put kind must register at least one overflow"
    );
    assert!(
        counter_u64_by(&points, OVERFLOW_METRIC, "kind", "delete").unwrap_or(0) >= 1,
        "delete kind must register at least one overflow"
    );

    // Release chunker BEFORE shutdown so the worker can drain.
    hold.store(false, Ordering::SeqCst);
    background.shutdown().await.unwrap();
}
