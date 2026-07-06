// Tests hold a Mutex guard across await points intentionally to serialize
// concurrent test runs that share global OTel state.
#![allow(clippy::await_holding_lock)]

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum_test::TestServer;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use serde::Serialize;
use serde_json::json;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig,
    TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_server::{
    ApiKeyAuthProvider, ApiKeyEntry, AppState, AuthError, AuthProvider, BudgetPoolConfig,
    Credentials, OidcAuthProvider, OidcConfig, SimulacraEngine, TaskManager, TaskState,
    TenantConfig, TenantResolver, build_router,
};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Debug, PartialEq)]
struct MetricPoint {
    metric: String,
    kind: MetricKind,
    attributes: BTreeMap<String, String>,
    value: MetricValue,
}

#[derive(Clone, Debug, PartialEq)]
enum MetricKind {
    Gauge,
    Sum,
    Histogram,
}

#[derive(Clone, Debug, PartialEq)]
enum MetricValue {
    GaugeI64(i64),
    GaugeU64(u64),
    GaugeF64(f64),
    SumI64(i64),
    SumU64(u64),
    SumF64(f64),
    HistogramI64 { count: u64, sum: i64 },
    HistogramU64 { count: u64, sum: u64 },
    HistogramF64 { count: u64, sum: f64 },
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
                    AggregatedMetrics::I64(MetricData::Gauge(gauge)) => {
                        for point in gauge.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Gauge,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::GaugeI64(point.value()),
                            });
                        }
                    }
                    AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                        for point in gauge.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Gauge,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::GaugeU64(point.value()),
                            });
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Gauge(gauge)) => {
                        for point in gauge.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Gauge,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::GaugeF64(point.value()),
                            });
                        }
                    }
                    AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                        for point in sum.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Sum,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::SumI64(point.value()),
                            });
                        }
                    }
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                        for point in sum.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Sum,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::SumU64(point.value()),
                            });
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Sum(sum)) => {
                        for point in sum.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Sum,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::SumF64(point.value()),
                            });
                        }
                    }
                    AggregatedMetrics::I64(MetricData::Histogram(histogram)) => {
                        for point in histogram.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Histogram,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::HistogramI64 {
                                    count: point.count(),
                                    sum: point.sum(),
                                },
                            });
                        }
                    }
                    AggregatedMetrics::U64(MetricData::Histogram(histogram)) => {
                        for point in histogram.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Histogram,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::HistogramU64 {
                                    count: point.count(),
                                    sum: point.sum(),
                                },
                            });
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                        for point in histogram.data_points() {
                            snapshot.push(MetricPoint {
                                metric: metric.name().to_string(),
                                kind: MetricKind::Histogram,
                                attributes: attrs(point.attributes()),
                                value: MetricValue::HistogramF64 {
                                    count: point.count(),
                                    sum: point.sum(),
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

    fn temporality(&self) -> opentelemetry_sdk::metrics::Temporality {
        opentelemetry_sdk::metrics::Temporality::Cumulative
    }
}

#[derive(Debug)]
struct TestTelemetry {
    meter_provider: SdkMeterProvider,
    metric_exporter: TestMetricExporter,
    tracer_provider: SdkTracerProvider,
    span_exporter: RecordingSpanExporter,
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

            let span_exporter = RecordingSpanExporter::default();
            let tracer_provider = SdkTracerProvider::builder()
                .with_simple_exporter(span_exporter.clone())
                .build();
            global::set_tracer_provider(tracer_provider.clone());

            let subscriber = tracing_subscriber::registry().with(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer_provider.tracer("simulacra-server-tests")),
            );
            let _ = tracing::subscriber::set_global_default(subscriber);

            TestTelemetry {
                meter_provider,
                metric_exporter,
                tracer_provider,
                span_exporter,
            }
        })
    }

    fn reset(&self) {
        self.metric_exporter.reset();
        self.span_exporter.reset();
    }

    fn flush_metrics(&self) -> Vec<MetricPoint> {
        self.meter_provider.force_flush().unwrap();
        self.metric_exporter.latest_points()
    }

    fn flush_spans(&self) -> Vec<opentelemetry_sdk::trace::SpanData> {
        self.tracer_provider.force_flush().unwrap();
        self.span_exporter.finished_spans()
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

fn tenant(namespace: &str, agent_type: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: agent_type.to_string(),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

fn engine_config(namespace: &str, agent_type: &str) -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        agent_type.to_string(),
        AgentTypeConfig {
            backend: Default::default(),
            model: "ollama:llama3".to_string(),
            acp_profile: None,
            system_prompt: Some("You are the worker.".to_string()),
            skills: vec![],
            max_turns: Some(12),
            max_tokens: Some(8_192),
            max_sub_agents: Some(0),
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec![],
                shell: false,
                javascript: false,
                python: false,
                paths_read: vec!["/**".to_string()],
                paths_write: vec!["/workspace/**".to_string()],
                skill_patterns: vec![],
                memory: None,
            }),
        },
    );

    let mut tenants = HashMap::new();
    tenants.insert(
        namespace.to_string(),
        SimulacraTenantConfig {
            agent_type: agent_type.to_string(),
            integrations: None,
            mcp_servers: Default::default(),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-server-observability-tests".to_string(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
        tenants,
        mcp: None,
        task: None,
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: CatalogConfig::default(),
    }
}

async fn api_key_server(namespace: &str, agent_type: &str) -> (Arc<TaskManager>, TestServer) {
    let t = tenant(namespace, agent_type);
    let mut tenants = HashMap::new();
    tenants.insert(namespace.to_string(), t.clone());

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants, None));
    let auth: Arc<dyn AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
            key: "key-a".to_string(),
            subject: "user-a".to_string(),
            tenant_namespace: Some(namespace.to_string()),
            scopes: vec!["tasks:manage".to_string()],
        }]));
    let engine = Arc::new(
        SimulacraEngine::new_with_in_memory_catalog(engine_config(namespace, agent_type), None)
            .await
            .unwrap(),
    );
    let state = AppState::with_engine(manager.clone(), resolver, auth, engine);
    let server = TestServer::builder()
        .http_transport()
        .build(build_router(state, vec![], None))
        .unwrap();

    (manager, server)
}

fn metric_points<'a>(metrics: &'a [MetricPoint], metric: &str) -> Vec<&'a MetricPoint> {
    metrics
        .iter()
        .filter(|point| point.metric == metric)
        .collect()
}

fn find_metric<'a>(
    metrics: &'a [MetricPoint],
    metric: &str,
    attrs: &[(&str, &str)],
) -> Option<&'a MetricPoint> {
    metric_points(metrics, metric).into_iter().find(|point| {
        attrs.iter().all(|(key, value)| {
            point
                .attributes
                .get(*key)
                .map(|candidate| candidate == value)
                .unwrap_or(false)
        })
    })
}

fn assert_gauge_value(
    metrics: &[MetricPoint],
    metric: &str,
    attrs: &[(&str, &str)],
    expected: i64,
) {
    let point = find_metric(metrics, metric, attrs).unwrap_or_else(|| {
        panic!("missing metric '{metric}' with attrs {attrs:?}; got {metrics:#?}");
    });
    assert_eq!(
        point.kind,
        MetricKind::Gauge,
        "{metric} must export as a gauge"
    );
    match point.value {
        MetricValue::GaugeI64(value) => assert_eq!(value, expected),
        MetricValue::GaugeU64(value) => assert_eq!(value as i64, expected),
        ref other => panic!("expected gauge value for '{metric}', got {other:?}"),
    }
}

fn assert_sum_value(metrics: &[MetricPoint], metric: &str, attrs: &[(&str, &str)], expected: u64) {
    let point = find_metric(metrics, metric, attrs).unwrap_or_else(|| {
        panic!("missing metric '{metric}' with attrs {attrs:?}; got {metrics:#?}");
    });
    assert_eq!(
        point.kind,
        MetricKind::Sum,
        "{metric} must export as a counter"
    );
    match point.value {
        MetricValue::SumU64(value) => assert_eq!(value, expected),
        ref other => panic!("expected u64 sum for '{metric}', got {other:?}"),
    }
}

fn assert_histogram_recorded(
    metrics: &[MetricPoint],
    terminal_state: &str,
    tenant: &str,
    agent_type: &str,
) {
    let point = find_metric(
        metrics,
        "simulacra.server.task_duration",
        &[
            ("tenant", tenant),
            ("agent_type", agent_type),
            ("terminal_state", terminal_state),
        ],
    )
    .unwrap_or_else(|| {
        panic!(
            "missing metric 'simulacra.server.task_duration' for terminal_state={terminal_state}; got {metrics:#?}"
        );
    });
    assert_eq!(
        point.kind,
        MetricKind::Histogram,
        "simulacra.server.task_duration must export as a histogram"
    );
    match point.value {
        MetricValue::HistogramF64 { count, sum } => {
            assert!(
                count >= 1,
                "histogram must record at least one terminal task"
            );
            assert!(sum >= 0.0, "task duration must be recorded in seconds");
        }
        ref other => panic!("expected f64 histogram for task_duration, got {other:?}"),
    }
}

#[derive(Clone, Default)]
struct RecordingSpanExporter {
    spans: Arc<Mutex<Vec<SpanData>>>,
}

impl std::fmt::Debug for RecordingSpanExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecordingSpanExporter")
    }
}

impl RecordingSpanExporter {
    fn finished_spans(&self) -> Vec<SpanData> {
        self.spans.lock().unwrap().clone()
    }

    fn reset(&self) {
        self.spans.lock().unwrap().clear();
    }
}

impl SpanExporter for RecordingSpanExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.spans.lock().unwrap().extend(batch);
        Ok(())
    }
}

fn span_attr<'a>(span: &'a opentelemetry_sdk::trace::SpanData, key: &str) -> Option<Cow<'a, str>> {
    span.attributes
        .iter()
        .find(|attr| attr.key.as_str() == key)
        .map(|attr| attr.value.as_str())
}

fn assert_span(spans: &[opentelemetry_sdk::trace::SpanData], name: &str, attrs: &[(&str, &str)]) {
    let span = spans.iter().find(|span| {
        span.name == name
            && attrs.iter().all(|(key, value)| {
                span_attr(span, key)
                    .map(|candidate| candidate == *value)
                    .unwrap_or(false)
            })
    });
    assert!(
        span.is_some(),
        "missing span '{name}' with attrs {attrs:?}; got spans {spans:#?}"
    );
}

fn encode_token(claims: impl Serialize, secret: &str) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn active_tasks_gauge_tracks_running_tasks_and_returns_to_zero() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let tenant = tenant("tenant-active-tasks", "agent-active-tasks");
    let manager = TaskManager::new();

    let first = manager
        .create_pending_task(&tenant, "first", None, json!({}), None)
        .unwrap();
    let second = manager
        .create_pending_task(&tenant, "second", None, json!({}), None)
        .unwrap();

    manager.start_task(&first.task_id).unwrap();
    manager.start_task(&second.task_id).unwrap();

    let running_metrics = telemetry.flush_metrics();
    assert_gauge_value(
        &running_metrics,
        "simulacra.server.active_tasks",
        &[("tenant", "tenant-active-tasks")],
        2,
    );

    manager
        .complete_task(
            &first.task_id,
            TaskState::Completed,
            Some("done".to_string()),
        )
        .unwrap();
    manager.cancel_task(&second.task_id).unwrap();

    let terminal_metrics = telemetry.flush_metrics();
    assert_gauge_value(
        &terminal_metrics,
        "simulacra.server.active_tasks",
        &[("tenant", "tenant-active-tasks")],
        0,
    );
}

#[tokio::test]
async fn active_connections_gauge_tracks_websocket_and_sse_connection_lifecycle() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let (manager, server) =
        api_key_server("tenant-active-connections", "agent-active-connections").await;
    let task = manager
        .create_task(
            &tenant("tenant-active-connections", "agent-active-connections"),
            "stream events",
            None,
            json!({}),
            None,
        )
        .unwrap();

    let websocket = server
        .get_websocket("/api/v1/ws")
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await
        .into_websocket()
        .await;

    let events_url = server
        .server_address()
        .unwrap()
        .join(&format!("/api/v1/tasks/{}/events", task.task_id))
        .unwrap();
    let sse_response = reqwest::Client::new()
        .get(events_url)
        .header("authorization", "ApiKey key-a")
        .send()
        .await
        .unwrap();
    assert_eq!(sse_response.status(), StatusCode::OK);

    let open_metrics = telemetry.flush_metrics();
    assert_gauge_value(
        &open_metrics,
        "simulacra.server.active_connections",
        &[("transport", "ws")],
        1,
    );
    assert_gauge_value(
        &open_metrics,
        "simulacra.server.active_connections",
        &[("transport", "sse")],
        1,
    );

    websocket.close().await;
    drop(sse_response);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let closed_metrics = telemetry.flush_metrics();
    assert_gauge_value(
        &closed_metrics,
        "simulacra.server.active_connections",
        &[("transport", "ws")],
        0,
    );
    assert_gauge_value(
        &closed_metrics,
        "simulacra.server.active_connections",
        &[("transport", "sse")],
        0,
    );
}

#[tokio::test]
async fn task_duration_histogram_records_each_terminal_state_with_labels() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let tenant = tenant("tenant-task-duration", "agent-task-duration");
    let manager = TaskManager::new();

    for terminal_state in [
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Killed,
        TaskState::Cancelled,
    ] {
        let handle = manager
            .create_pending_task(
                &tenant,
                format!("task-{terminal_state}"),
                None,
                json!({}),
                None,
            )
            .unwrap();
        manager.start_task(&handle.task_id).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        manager
            .complete_task(
                &handle.task_id,
                terminal_state.clone(),
                Some("terminal".to_string()),
            )
            .unwrap();
    }

    let metrics = telemetry.flush_metrics();
    assert_histogram_recorded(
        &metrics,
        "completed",
        "tenant-task-duration",
        "agent-task-duration",
    );
    assert_histogram_recorded(
        &metrics,
        "failed",
        "tenant-task-duration",
        "agent-task-duration",
    );
    assert_histogram_recorded(
        &metrics,
        "killed",
        "tenant-task-duration",
        "agent-task-duration",
    );
    assert_histogram_recorded(
        &metrics,
        "cancelled",
        "tenant-task-duration",
        "agent-task-duration",
    );
}

#[tokio::test]
async fn events_emitted_counter_increments_per_event_with_event_type_and_tenant() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let tenant = tenant("tenant-events-emitted", "agent-events-emitted");
    let manager = TaskManager::new();
    let handle = manager
        .create_task(&tenant, "emit events", None, json!({}), None)
        .unwrap();

    manager
        .emit_event(
            &handle.task_id,
            json!({
                "event": "tool.called",
                "tool_name": "shell_exec",
            }),
        )
        .unwrap();
    manager
        .emit_event(
            &handle.task_id,
            json!({
                "event": "tool.called",
                "tool_name": "shell_exec",
            }),
        )
        .unwrap();
    manager
        .emit_event(
            &handle.task_id,
            json!({
                "event": "agent.message",
                "content": "hello",
            }),
        )
        .unwrap();

    let metrics = telemetry.flush_metrics();
    assert_sum_value(
        &metrics,
        "simulacra.server.events_emitted",
        &[
            ("event_type", "tool.called"),
            ("tenant", "tenant-events-emitted"),
        ],
        2,
    );
    assert_sum_value(
        &metrics,
        "simulacra.server.events_emitted",
        &[
            ("event_type", "agent.message"),
            ("tenant", "tenant-events-emitted"),
        ],
        1,
    );
}

#[tokio::test]
async fn auth_failures_counter_uses_reason_labels_for_all_autherror_variants() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let api_key = ApiKeyAuthProvider::from_entries(vec![ApiKeyEntry {
        key: "known-key".to_string(),
        subject: "svc".to_string(),
        tenant_namespace: Some("tenant-auth".to_string()),
        scopes: vec!["tasks:manage".to_string()],
    }]);
    let oidc = OidcAuthProvider::with_key(
        OidcConfig {
            issuer: "https://issuer.example".to_string(),
            audience: "simulacra-api".to_string(),
            tenant_claim: "tenant".to_string(),
            jwks_url: None,
        },
        jsonwebtoken::DecodingKey::from_secret("correct-secret".as_bytes()),
    );

    let now = chrono::Utc::now().timestamp();
    let expired_token = encode_token(
        json!({
            "sub": "user-1",
            "iss": "https://issuer.example",
            "aud": "simulacra-api",
            "tenant": "tenant-auth",
            "exp": now - 60,
        }),
        "correct-secret",
    );
    let invalid_signature_token = encode_token(
        json!({
            "sub": "user-1",
            "iss": "https://issuer.example",
            "aud": "simulacra-api",
            "tenant": "tenant-auth",
            "exp": now + 3600,
        }),
        "wrong-secret",
    );

    let unauthorized = api_key
        .authenticate(&Credentials::Bearer("wrong-type".to_string()))
        .await;
    let expired = oidc.authenticate(&Credentials::Bearer(expired_token)).await;
    let invalid_signature = oidc
        .authenticate(&Credentials::Bearer(invalid_signature_token))
        .await;
    let missing_credentials = api_key
        .authenticate(&Credentials::ApiKey(String::new()))
        .await;

    assert!(matches!(unauthorized, Err(AuthError::Unauthorized)));
    assert!(matches!(expired, Err(AuthError::Expired)));
    assert!(matches!(
        invalid_signature,
        Err(AuthError::InvalidSignature)
    ));
    assert!(matches!(
        missing_credentials,
        Err(AuthError::MissingCredentials)
    ));

    let metrics = telemetry.flush_metrics();
    assert_sum_value(
        &metrics,
        "simulacra.server.auth_failures",
        &[("provider", "api_key"), ("reason", "unauthorized")],
        1,
    );
    assert_sum_value(
        &metrics,
        "simulacra.server.auth_failures",
        &[("provider", "oidc"), ("reason", "expired")],
        1,
    );
    assert_sum_value(
        &metrics,
        "simulacra.server.auth_failures",
        &[("provider", "oidc"), ("reason", "invalid_signature")],
        1,
    );
    assert_sum_value(
        &metrics,
        "simulacra.server.auth_failures",
        &[("provider", "api_key"), ("reason", "missing_credentials")],
        1,
    );
}

#[tokio::test]
async fn simulacra_server_request_span_contains_method_path_and_tenant_attributes() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let namespace = "tenant-request-span";
    let agent_type = "agent-request-span";
    let (manager, server) = api_key_server(namespace, agent_type).await;
    let task = manager
        .create_task(
            &tenant(namespace, agent_type),
            "status target",
            None,
            json!({}),
            None,
        )
        .unwrap();

    let response = server
        .get(&format!("/api/v1/tasks/{}/status", task.task_id))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("ApiKey key-a"),
        )
        .await;
    response.assert_status_ok();

    let spans = telemetry.flush_spans();
    assert_span(
        &spans,
        "simulacra_server_request",
        &[
            ("simulacra.server.method", "GET"),
            (
                "simulacra.server.path",
                &format!("/api/v1/tasks/{}/status", task.task_id),
            ),
            ("simulacra.server.tenant", namespace),
        ],
    );
}

#[tokio::test]
async fn simulacra_server_task_span_contains_task_id_agent_type_and_tenant_attributes() {
    let _guard = test_lock();
    let telemetry = TestTelemetry::install();
    telemetry.reset();

    let tenant = tenant("tenant-task-span", "agent-task-span");
    let manager = TaskManager::new();
    let handle = manager
        .create_pending_task(&tenant, "tracked task", None, json!({}), None)
        .unwrap();

    manager.start_task(&handle.task_id).unwrap();
    manager
        .complete_task(
            &handle.task_id,
            TaskState::Completed,
            Some("done".to_string()),
        )
        .unwrap();

    let spans = telemetry.flush_spans();
    assert_span(
        &spans,
        "simulacra_server_task",
        &[
            ("simulacra.server.task_id", &handle.task_id),
            ("simulacra.server.agent_type", "agent-task-span"),
            ("simulacra.server.tenant", "tenant-task-span"),
        ],
    );
}
