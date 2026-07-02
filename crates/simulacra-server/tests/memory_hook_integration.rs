//! S037 §20 Hook integration tests — memory tools must consult the `tool_call` hook pipeline.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, MemoryCapabilityConfig, ProjectConfig,
    SimulacraConfig, TenantConfig as SimulacraTenantConfig, VfsConfig,
};
use simulacra_hooks::{HookError, HookModule, HookPipeline, Operation, Phase, Verdict};
use simulacra_memory::{
    DefaultEmbedder, Embedder, HitIdCache, IndexedChunk, MemoryStore, SqliteMemoryStore,
    SqliteVectorIndex, VectorIndex,
};
use simulacra_server::{LocalDiskArtifactStore, SimulacraEngine};
use simulacra_tool::{MemoryToolHandles, ToolRegistry, register_memory_tools};
use simulacra_types::{
    ArtifactStore, CapabilityToken, Locator, MemoryCapability, MemoryPath, MemoryVersion, TenantId,
    ToolError,
};
use tempfile::TempDir;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::SubscriberExt;

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

fn build_registry(harness: &Harness, hook_pipeline: Option<Arc<HookPipeline>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_memory_tools(
        &mut registry,
        MemoryToolHandles {
            tenant: harness.tenant.clone(),
            capability: harness.capability.clone(),
            memory_store: Arc::clone(&harness.memory_store),
            vector_index: Arc::clone(&harness.vector_index),
            embedder: Arc::clone(&harness.embedder),
            hit_cache: Arc::clone(&harness.hit_cache),
            rrwb: None,
            hook_pipeline,
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

fn seed_chunk(harness: &Harness, path_str: &str, text: &str) -> MemoryVersion {
    let path = MemoryPath::parse(path_str).unwrap();
    let version = harness
        .memory_store
        .put(&harness.tenant, &path, text.as_bytes())
        .unwrap();
    let embedding = harness
        .embedder
        .embed(&[text])
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let chunk = IndexedChunk {
        chunk_index: 0,
        locator: Locator::Text {
            byte_start: 0,
            byte_end: text.len(),
        },
        text: text.to_string(),
        embedding,
    };
    harness
        .vector_index
        .upsert(
            &harness.tenant,
            &path,
            version,
            harness.embedder.id(),
            &[chunk],
        )
        .unwrap();
    version
}

/// Drives a `semantic_search` invocation against a hook-free registry so a
/// caller can resolve a hit id for subsequent `memory_read_chunk` tests. B3:
/// was previously `fn` that used `block_in_place` + `Handle::current().block_on`;
/// that pattern is fragile inside an already-async test. Prefer `.await`.
async fn search_hit_id(harness: &Harness, query: &str) -> String {
    let registry = build_registry(harness, None);
    let token = capability_token(harness.capability.clone());
    let result = registry
        .call(
            "semantic_search",
            json!({
                "query": query,
                "scope": "/var/memory/self",
                "k": 10,
            }),
            &token,
        )
        .await
        .unwrap();

    result["hits"][0]["hit_id"].as_str().unwrap().to_string()
}

fn rewrite_json(context: &str, update: impl FnOnce(&mut Value)) -> String {
    let mut value: Value = serde_json::from_str(context).unwrap();
    update(&mut value);
    value.to_string()
}

/// A hook that returns `Verdict::Deny` in the **before** phase only. We
/// explicitly exclude the after phase because `HookPipeline::run_after`
/// downgrades `Deny` to `Continue` (see `pipeline.rs` — it logs a warning
/// but does not short-circuit). Any future "after-phase deny" test must
/// therefore use a different pattern (or a different verdict such as
/// `Kill`); this hook would silently no-op in after-phase.
struct BeforeDenyHook {
    name: String,
    operation: Operation,
}

impl HookModule for BeforeDenyHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        if phase == Phase::Before && operation == self.operation {
            Ok(Verdict::Deny("test deny".to_string()))
        } else {
            Ok(Verdict::Continue(None))
        }
    }
}

/// A hook that returns `Err(HookError::Timeout { .. })` in the **before**
/// phase. Per `pipeline.rs::run_before`, timeouts are converted to a
/// `Verdict::Deny` with a fail-closed reason. Mirrors production JS hook
/// behaviour where a misbehaving hook that never resolves its promise
/// within the configured timeout should block the tool call.
struct BeforeTimeoutHook {
    name: String,
    timeout_ms: u64,
}

impl HookModule for BeforeTimeoutHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        if phase == Phase::Before {
            Err(HookError::Timeout {
                hook: self.name.clone(),
                timeout_ms: self.timeout_ms,
            })
        } else {
            Ok(Verdict::Continue(None))
        }
    }
}

/// A hook that returns `Verdict::Kill` in the **before** phase. Per
/// `pipeline.rs::run_before`, Kill is surfaced as `Err(HookError::Killed)`
/// — the tool call should surface a `ToolError::ExecutionFailed` rather
/// than a graceful `{hits: []}` or `{error: "denied"}` shape.
struct BeforeKillHook {
    name: String,
}

impl HookModule for BeforeKillHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        if phase == Phase::Before {
            Ok(Verdict::Kill("test kill".to_string()))
        } else {
            Ok(Verdict::Continue(None))
        }
    }
}

struct RecordingHook {
    name: String,
    captured: Arc<Mutex<Vec<(Phase, String)>>>,
}

impl HookModule for RecordingHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        // W2: defensively only record `ToolCall` invocations. If a future
        // test accidentally registers this hook on multiple operations, we
        // do not want cross-op captures polluting the assertions below
        // (e.g. latest_context(... Phase::Before) returning the wrong op).
        if operation != Operation::ToolCall {
            return Ok(Verdict::Continue(None));
        }
        self.captured
            .lock()
            .unwrap()
            .push((phase, context.to_string()));
        Ok(Verdict::Continue(None))
    }
}

type TransformFn = dyn Fn(Phase, &str) -> String + Send + Sync;

struct MutatingHook {
    name: String,
    transform: Arc<TransformFn>,
}

impl HookModule for MutatingHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        Ok(Verdict::Continue(Some((self.transform)(phase, context))))
    }
}

fn pipeline_with_hook(hook: Arc<dyn HookModule>) -> Arc<HookPipeline> {
    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook);
    Arc::new(pipeline)
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    current_span: Option<String>,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
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

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        event.record(&mut visitor);
        self.events.lock().unwrap().push(CapturedEvent {
            current_span: ctx.lookup_current().map(|span| span.name().to_string()),
            fields,
        });
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

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

/// Runs an async operation under a temporary subscriber and captures span/event fields.
///
/// B3 note: uses `WithSubscriber` to wrap the future so every poll re-enters
/// the test-local subscriber. That keeps capture correctness when the future
/// is resumed on different worker threads (the `block_in_place` +
/// `Handle::current().block_on` pattern the draft used is fragile inside an
/// already-async executor). `Span::record` calls dispatched during the
/// future's execution land in `CaptureLayer::on_record`, which is how
/// post-hook span attribute assertions succeed.
async fn capture_with_subscriber<F, Fut, T>(
    operation: F,
) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });

    let result = operation().with_subscriber(subscriber).await;

    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

fn span_named<'a>(spans: &'a [CapturedSpan], name: &str) -> &'a CapturedSpan {
    spans
        .iter()
        .rev()
        .find(|span| span.name == name)
        .unwrap_or_else(|| panic!("missing span {name}; spans={spans:?}"))
}

fn latest_context(captured: &Arc<Mutex<Vec<(Phase, String)>>>, phase: Phase) -> Value {
    let entries = captured.lock().unwrap();
    let context = entries
        .iter()
        .rev()
        .find(|(recorded_phase, _)| *recorded_phase == phase)
        .unwrap_or_else(|| panic!("missing {phase} hook context; captured={entries:?}"))
        .1
        .clone();
    serde_json::from_str(&context).unwrap()
}

fn span_contains_value(span: &CapturedSpan, needle: &str) -> bool {
    span.fields.values().any(|value| value.contains(needle))
}

#[allow(dead_code)] // intentionally unused — see W3 note on relaxed observability
fn events_mention(events: &[CapturedEvent], needle: &str) -> bool {
    events.iter().any(|event| {
        event
            .current_span
            .as_deref()
            .is_some_and(|span| span.contains(needle))
            || event.fields.values().any(|value| value.contains(needle))
    })
}

fn engine_config() -> SimulacraConfig {
    let mut agent_types = HashMap::new();
    agent_types.insert(
        "worker".to_string(),
        AgentTypeConfig {
            model: "ollama:llama3".to_string(),
            system_prompt: Some("You are the worker.".to_string()),
            skills: vec![],
            max_turns: Some(8),
            max_tokens: Some(4_096),
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
                memory: Some(MemoryCapabilityConfig {
                    enabled: true,
                    search_scopes: vec!["/var/memory/self".to_string()],
                    write_scopes: vec!["/var/memory/self".to_string()],
                }),
            }),
        },
    );

    let mut tenants = HashMap::new();
    tenants.insert(
        "acme".to_string(),
        SimulacraTenantConfig {
            agent_type: "worker".to_string(),
            integrations: None,
            mcp_servers: Default::default(),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-memory-hook-tests".to_string(),
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

// ─── semantic_search ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_hook_denial_returns_empty_hits_and_denied_span() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires pulling customer data",
    );
    let pipeline = pipeline_with_hook(Arc::new(BeforeDenyHook {
        name: "deny-search".to_string(),
        operation: Operation::ToolCall,
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let (result, spans, _) = capture_with_subscriber(|| async {
        registry
            .call(
                "semantic_search",
                json!({
                    "query": "quarterly close",
                    "scope": "/var/memory/self",
                }),
                &token,
            )
            .await
            .unwrap()
    })
    .await;

    assert_eq!(result, json!({ "hits": [] }));
    let span = span_named(&spans, "memory_search");
    assert_eq!(
        span.fields.get("memory.search.denied").map(String::as_str),
        Some("true"),
        "{span:?}"
    );
}

// S037 §20 line 1171 + §9.3: a hook that redacts the result must propagate
// the redaction to both the returned payload AND the post-hook span
// attributes (`memory.query`, `memory.hit_paths`). Pre-redaction snippet
// content must NOT appear in the span — no DLP bypass via observability.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_after_hook_redacts_snippet_in_result_and_span() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "SSN 123-45-6789 appears in the quarterly close checklist",
    );
    let pipeline = pipeline_with_hook(Arc::new(MutatingHook {
        name: "redact-snippet".to_string(),
        transform: Arc::new(|phase, context| {
            if phase == Phase::After {
                rewrite_json(context, |value| {
                    value["result"]["hits"][0]["snippet"] = json!("[REDACTED SNIPPET]");
                })
            } else {
                context.to_string()
            }
        }),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let (result, spans, _) = capture_with_subscriber(|| async {
        registry
            .call(
                "semantic_search",
                json!({
                    "query": "quarterly close SSN",
                    "scope": "/var/memory/self",
                }),
                &token,
            )
            .await
            .unwrap()
    })
    .await;

    assert_eq!(result["hits"][0]["snippet"], json!("[REDACTED SNIPPET]"));
    let span = span_named(&spans, "memory_search");
    // Per §9.3: `memory.hit_paths` is recorded POST-hook. A hook that
    // only rewrites snippets (not paths) leaves the path in the span —
    // but the pre-redaction *snippet content* must never appear anywhere
    // in the span (DLP invariant).
    assert!(
        span.fields.contains_key("memory.hit_paths"),
        "span must carry post-hook memory.hit_paths: {span:?}"
    );
    assert!(
        !span_contains_value(span, "123-45-6789"),
        "pre-redaction PII must not leak into span: {span:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_after_hook_can_drop_hits_from_result_and_logs() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/keep.md",
        "quarterly close playbook for the finance team",
    );
    seed_chunk(
        &harness,
        "/var/memory/self/drop.md",
        "quarterly close draft containing raw payroll details",
    );
    let dropped_path = "/var/memory/self/drop.md";
    let pipeline = pipeline_with_hook(Arc::new(MutatingHook {
        name: "drop-hit".to_string(),
        transform: Arc::new(move |phase, context| {
            if phase == Phase::After {
                rewrite_json(context, |value| {
                    let hits = value["result"]["hits"].as_array_mut().unwrap();
                    hits.retain(|hit| hit["path"] != json!(dropped_path));
                })
            } else {
                context.to_string()
            }
        }),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    // W3: we intentionally do NOT assert the dropped path is absent from
    // all tracing events. Observability layers may legitimately log the
    // pre-hook hit set for audit/debug purposes. The post-hook PAYLOAD
    // must not contain the dropped path — that is the spec contract.
    let result = registry
        .call(
            "semantic_search",
            json!({
                "query": "quarterly close",
                "scope": "/var/memory/self",
                "k": 10,
            }),
            &token,
        )
        .await
        .unwrap();

    let hits = result["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{result}");
    assert!(
        hits.iter()
            .all(|hit| hit["path"].as_str() != Some(dropped_path)),
        "{result}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn simulacra_engine_exposes_shared_hook_pipeline_for_memory_wiring() {
    let tmp = tempfile::tempdir().unwrap();
    let artifact_store: Arc<dyn ArtifactStore> =
        Arc::new(LocalDiskArtifactStore::new(tmp.path()).unwrap());
    let memory_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(tmp.path()).unwrap());
    let embedder: Arc<dyn Embedder> = Arc::new(DefaultEmbedder::load_default().unwrap());
    let vector_index: Arc<dyn VectorIndex> =
        Arc::new(SqliteVectorIndex::new(tmp.path(), embedder.id().clone()).unwrap());

    let engine = SimulacraEngine::with_memory_in_memory_catalog(
        engine_config(),
        None,
        artifact_store,
        memory_store,
        vector_index,
        embedder,
    )
    .await
    .unwrap();

    let pipeline = Arc::clone(engine.hook_pipeline());
    assert!(Arc::ptr_eq(&pipeline, engine.hook_pipeline()));
    assert!(Arc::strong_count(&pipeline) >= 2);
    assert_eq!(
        pipeline.hook_names(Operation::ToolCall),
        Vec::<String>::new()
    );
}

/// W4: proxy for "set_hook_pipeline wires the pipeline into tasks spawned by
/// spawn_task". Running a full agent loop requires an LLM; instead we verify
/// that (a) `set_hook_pipeline` replaces the engine's Arc, (b) the new Arc is
/// the one returned by `hook_pipeline()`, and (c) the hook is visible under
/// `Operation::ToolCall`. Phase 2 must thread this same Arc through
/// `MemoryToolHandles::hook_pipeline` at `spawn_task`.
#[tokio::test(flavor = "multi_thread")]
async fn simulacra_engine_set_hook_pipeline_replaces_shared_pipeline_arc() {
    let tmp = tempfile::tempdir().unwrap();
    let artifact_store: Arc<dyn ArtifactStore> =
        Arc::new(LocalDiskArtifactStore::new(tmp.path()).unwrap());
    let memory_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(tmp.path()).unwrap());
    let embedder: Arc<dyn Embedder> = Arc::new(DefaultEmbedder::load_default().unwrap());
    let vector_index: Arc<dyn VectorIndex> =
        Arc::new(SqliteVectorIndex::new(tmp.path(), embedder.id().clone()).unwrap());

    let mut engine = SimulacraEngine::with_memory_in_memory_catalog(
        engine_config(),
        None,
        artifact_store,
        memory_store,
        vector_index,
        embedder,
    )
    .await
    .unwrap();

    let initial_pipeline_ptr = Arc::as_ptr(engine.hook_pipeline());

    let custom_pipeline = pipeline_with_hook(Arc::new(BeforeDenyHook {
        name: "engine-wired-deny".to_string(),
        operation: Operation::ToolCall,
    }));
    let custom_pipeline_ptr = Arc::as_ptr(&custom_pipeline);

    engine.set_hook_pipeline(Arc::clone(&custom_pipeline));

    // The engine's pipeline Arc must now point at the replacement, not the
    // default-constructed one. This is what spawn_task clones into
    // `MemoryToolHandles::hook_pipeline`.
    let engine_ptr = Arc::as_ptr(engine.hook_pipeline());
    assert_ne!(engine_ptr, initial_pipeline_ptr);
    assert_eq!(engine_ptr, custom_pipeline_ptr);
    assert!(Arc::ptr_eq(engine.hook_pipeline(), &custom_pipeline));

    // The hook is visible through the engine-held Arc. If Phase 2 accidentally
    // gives the memory tools a different pipeline (e.g. a fresh
    // `HookPipeline::new()` per spawn_task call), this list would be empty.
    assert_eq!(
        engine.hook_pipeline().hook_names(Operation::ToolCall),
        vec!["engine-wired-deny".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_before_hook_receives_query_context() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires finance approval",
    );
    let captured = Arc::new(Mutex::new(Vec::new()));
    let pipeline = pipeline_with_hook(Arc::new(RecordingHook {
        name: "record-search".to_string(),
        captured: Arc::clone(&captured),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    registry
        .call(
            "semantic_search",
            json!({
                "query": "quarterly close",
                "scope": "/var/memory/self",
                "k": 3,
            }),
            &token,
        )
        .await
        .unwrap();

    let before = latest_context(&captured, Phase::Before);
    assert_eq!(before["tool"], json!("semantic_search"));
    assert_eq!(before["arguments"]["query"], json!("quarterly close"));
    assert_eq!(before["arguments"]["scope"], json!("/var/memory/self"));
    assert_eq!(before["arguments"]["k"], json!(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_after_hook_receives_hits_array() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires finance approval",
    );
    let captured = Arc::new(Mutex::new(Vec::new()));
    let pipeline = pipeline_with_hook(Arc::new(RecordingHook {
        name: "record-search".to_string(),
        captured: Arc::clone(&captured),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    registry
        .call(
            "semantic_search",
            json!({
                "query": "quarterly close",
                "scope": "/var/memory/self",
            }),
            &token,
        )
        .await
        .unwrap();

    let after = latest_context(&captured, Phase::After);
    assert_eq!(after["tool"], json!("semantic_search"));
    assert!(after["result"]["hits"].is_array(), "{after}");
    assert!(
        !after["result"]["hits"].as_array().unwrap().is_empty(),
        "{after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_span_hit_count_uses_post_hook_hit_count() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/keep.md",
        "quarterly close playbook for the finance team",
    );
    seed_chunk(
        &harness,
        "/var/memory/self/drop.md",
        "quarterly close draft containing raw payroll details",
    );
    let pipeline = pipeline_with_hook(Arc::new(MutatingHook {
        name: "drop-hit".to_string(),
        transform: Arc::new(|phase, context| {
            if phase == Phase::After {
                rewrite_json(context, |value| {
                    let hits = value["result"]["hits"].as_array_mut().unwrap();
                    hits.retain(|hit| hit["path"] != json!("/var/memory/self/drop.md"));
                })
            } else {
                context.to_string()
            }
        }),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let (result, spans, _) = capture_with_subscriber(|| async {
        registry
            .call(
                "semantic_search",
                json!({
                    "query": "quarterly close",
                    "scope": "/var/memory/self",
                    "k": 10,
                }),
                &token,
            )
            .await
            .unwrap()
    })
    .await;

    assert_eq!(result["hits"].as_array().unwrap().len(), 1, "{result}");
    let span = span_named(&spans, "memory_search");
    assert_eq!(
        span.fields.get("memory.hit_count").map(String::as_str),
        Some("1")
    );
}

// ─── memory_read_chunk ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn memory_read_chunk_hook_denial_returns_403_payload() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires pulling customer data",
    );
    // Design note: `memory_read_chunk`'s before-hook fires AFTER hit id
    // resolution and AFTER the 404 (missing hit) / 410 (stale/deleted)
    // TOCTOU checks. That means a deny verdict reaches the caller only
    // when the hit is fresh and in-scope. If the hit is stale or missing,
    // the tool returns the error WITHOUT consulting the hook pipeline —
    // see `memory_read_chunk_capability_denied_bypasses_hook_pipeline`
    // below for the capability-gate variant.
    let hit_id = search_hit_id(&harness, "quarterly close").await;
    let pipeline = pipeline_with_hook(Arc::new(BeforeDenyHook {
        name: "deny-read".to_string(),
        operation: Operation::ToolCall,
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let result = registry
        .call("memory_read_chunk", json!({ "hit_id": hit_id }), &token)
        .await
        .unwrap();

    assert_eq!(
        result,
        json!({
            "error": "denied",
            "code": 403,
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_read_chunk_before_hook_receives_resolved_coordinates() {
    let harness = build_harness();
    let version = seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires pulling customer data",
    );
    let hit_id = search_hit_id(&harness, "quarterly close").await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let pipeline = pipeline_with_hook(Arc::new(RecordingHook {
        name: "record-read".to_string(),
        captured: Arc::clone(&captured),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    registry
        .call("memory_read_chunk", json!({ "hit_id": hit_id }), &token)
        .await
        .unwrap();

    let before = latest_context(&captured, Phase::Before);
    assert_eq!(before["tool"], json!("memory_read_chunk"));
    assert_eq!(
        before["arguments"]["path"],
        json!("/var/memory/self/note.md")
    );
    assert_eq!(before["arguments"]["chunk_index"], json!(0));
    assert_eq!(before["arguments"]["version"], json!(version.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_read_chunk_after_hook_can_redact_returned_content() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "SSN 123-45-6789 appears in the quarterly close checklist",
    );
    let hit_id = search_hit_id(&harness, "quarterly close SSN").await;
    let pipeline = pipeline_with_hook(Arc::new(MutatingHook {
        name: "redact-content".to_string(),
        transform: Arc::new(|phase, context| {
            if phase == Phase::After {
                rewrite_json(context, |value| {
                    value["result"]["content"] = json!("[REDACTED CHUNK]");
                })
            } else {
                context.to_string()
            }
        }),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let result = registry
        .call("memory_read_chunk", json!({ "hit_id": hit_id }), &token)
        .await
        .unwrap();

    assert_eq!(result["content"], json!("[REDACTED CHUNK]"));
}

// B2 note: this test verifies that `memory_read_chunk` re-records
// span attributes from the post-hook `result` object rather than from
// the pre-hook entry resolved out of `HitIdCache`. Mutating `arguments`
// in the after-phase is not a contract-supported operation because the
// tool has already executed (the after-phase pipeline may chain
// `arguments` but the tool ignores it). The Phase 2 implementation must
// therefore re-read `result["path"]` from the hook-mutated context and
// call `span.record("memory.path", ...)` at that point — which is what
// `CaptureLayer::on_record` captures.
//
// Also see the design note near `memory_read_chunk_hook_denial_returns_403_payload`:
// before-phase hook fires AFTER hit resolution and AFTER the 404/410
// TOCTOU checks, so a deny in before-phase still returns `{error:
// "denied", code: 403}` (not 404/410) if the hit exists and is fresh.
#[tokio::test(flavor = "multi_thread")]
async fn memory_read_chunk_span_path_reflects_post_hook_result_path() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires pulling customer data",
    );
    let hit_id = search_hit_id(&harness, "quarterly close").await;
    let pipeline = pipeline_with_hook(Arc::new(MutatingHook {
        name: "rewrite-result-path".to_string(),
        transform: Arc::new(|phase, context| {
            if phase == Phase::After {
                rewrite_json(context, |value| {
                    value["result"]["path"] = json!("/var/memory/self/redacted.md");
                })
            } else {
                context.to_string()
            }
        }),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let (result, spans, _) = capture_with_subscriber(|| async {
        registry
            .call("memory_read_chunk", json!({ "hit_id": hit_id }), &token)
            .await
            .unwrap()
    })
    .await;

    // Sanity: the payload we return to the caller reflects the after-phase
    // mutation. If this fails, the tool is not routing `result` through
    // `run_after` at all.
    assert_eq!(result["path"], json!("/var/memory/self/redacted.md"));

    // Core contract: the span's `memory.path` field must reflect the
    // post-hook `result["path"]` rather than the pre-hook entry path that
    // was resolved out of `HitIdCache`.
    //
    // Design observation: `Span::record()` calls (not just span creation)
    // drive `CaptureLayer::on_record`. For this assertion to pass, the
    // Phase 2 implementation MUST call `span.record("memory.path", ...)`
    // after extracting the final result from the after-phase hook context
    // — a one-time initialization at span creation is not enough.
    let span = span_named(&spans, "memory_read_chunk");
    assert_eq!(
        span.fields.get("memory.path").map(String::as_str),
        Some("/var/memory/self/redacted.md"),
        "{span:?}"
    );
}

// ─── Kill / Timeout / mutated-query / out-of-scope edge cases ───────────────

/// Missing edge case #1: a before-phase `Verdict::Kill` on semantic_search
/// must surface as `Err(ToolError::ExecutionFailed)` with the killed-hook
/// message, NOT a graceful `{hits: []}`. Per pipeline.rs::run_before, Kill
/// is returned as `Err(HookError::Killed)` and the ToolRegistry (or the
/// memory tool's internal pipeline invocation) maps that to
/// `ToolError::ExecutionFailed("hook error: ...")`.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_before_hook_kill_verdict_returns_err() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires pulling customer data",
    );
    let pipeline = pipeline_with_hook(Arc::new(BeforeKillHook {
        name: "kill-search".to_string(),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let err = registry
        .call(
            "semantic_search",
            json!({
                "query": "quarterly close",
                "scope": "/var/memory/self",
            }),
            &token,
        )
        .await
        .expect_err("Kill verdict must surface as Err, not a graceful payload");

    match err {
        ToolError::ExecutionFailed(msg) => {
            assert!(
                msg.contains("kill") || msg.contains("Killed") || msg.contains("hook"),
                "unexpected message: {msg}"
            );
        }
        other => panic!("expected ExecutionFailed, got {other:?}"),
    }
}

/// Missing edge case #2: same as above but for `memory_read_chunk`.
#[tokio::test(flavor = "multi_thread")]
async fn memory_read_chunk_before_hook_kill_verdict_returns_err() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires pulling customer data",
    );
    let hit_id = search_hit_id(&harness, "quarterly close").await;
    let pipeline = pipeline_with_hook(Arc::new(BeforeKillHook {
        name: "kill-read".to_string(),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let err = registry
        .call("memory_read_chunk", json!({ "hit_id": hit_id }), &token)
        .await
        .expect_err("Kill verdict must surface as Err, not {error: \"denied\"}");

    match err {
        ToolError::ExecutionFailed(msg) => {
            assert!(
                msg.contains("kill") || msg.contains("Killed") || msg.contains("hook"),
                "unexpected message: {msg}"
            );
        }
        other => panic!("expected ExecutionFailed, got {other:?}"),
    }
}

/// Missing edge case #3: a before-phase `Err(HookError::Timeout {...})` is
/// converted to a fail-closed `Verdict::Deny` by `pipeline.rs::run_before`
/// (see lines ~138-156). For semantic_search, the deny path returns the
/// spec-mandated `{hits: []}` shape and records `memory.search.denied=true`
/// on the span — identical to an explicit before-phase deny. This proves
/// the tool's deny handler treats
/// timeout-originated denies as first-class denials rather than propagating
/// the raw timeout error.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_before_hook_timeout_surfaces_as_denied() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close requires pulling customer data",
    );
    let pipeline = pipeline_with_hook(Arc::new(BeforeTimeoutHook {
        name: "slow-search".to_string(),
        timeout_ms: 5_000,
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let (result, spans, _) = capture_with_subscriber(|| async {
        registry
            .call(
                "semantic_search",
                json!({
                    "query": "quarterly close",
                    "scope": "/var/memory/self",
                }),
                &token,
            )
            .await
            .unwrap()
    })
    .await;

    assert_eq!(result, json!({ "hits": [] }));
    let span = span_named(&spans, "memory_search");
    assert_eq!(
        span.fields.get("memory.search.denied").map(String::as_str),
        Some("true"),
        "{span:?}"
    );
}

/// Missing edge case #4: a before-phase hook rewrites `arguments.query`
/// via `Continue(Some(modified_ctx))`. The embedding must be computed
/// against the MUTATED query — we seed two documents with disjoint
/// content, the user asks about A, the hook rewrites the query to look
/// for B, and we assert the result hits correspond to B not A. This is
/// the tightest behavioral proxy for "embedding computed against the
/// mutated query" without poking at the embedder directly.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_before_hook_mutates_query_and_embedding_uses_mutated_query() {
    let harness = build_harness();
    // Document A — what the user asks about (should NOT be returned).
    seed_chunk(
        &harness,
        "/var/memory/self/payroll.md",
        "payroll schedule and tax withholding for Q2",
    );
    // Document B — what the hook redirects to (should be returned).
    seed_chunk(
        &harness,
        "/var/memory/self/retention.md",
        "employee retention survey results and morale summary",
    );

    let pipeline = pipeline_with_hook(Arc::new(MutatingHook {
        name: "rewrite-query".to_string(),
        transform: Arc::new(|phase, context| {
            if phase == Phase::Before {
                rewrite_json(context, |value| {
                    value["arguments"]["query"] = json!("employee retention morale survey results");
                })
            } else {
                context.to_string()
            }
        }),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let result = registry
        .call(
            "semantic_search",
            json!({
                "query": "payroll withholding tax",
                "scope": "/var/memory/self",
                "k": 1,
            }),
            &token,
        )
        .await
        .unwrap();

    let hits = result["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{result}");
    // The top hit should be the retention doc (what the hook redirected
    // to), not the payroll doc (what the user originally asked about).
    assert_eq!(
        hits[0]["path"],
        json!("/var/memory/self/retention.md"),
        "embedding was computed against the ORIGINAL query instead of the \
         hook-mutated query — result: {result}"
    );
}

/// Missing edge case #5: capability checks inside `memory_read_chunk`
/// happen BEFORE any hook invocation. A hit_id minted for an out-of-scope
/// path is rejected with `{error: "hit_not_found", code: 404}` (see
/// `memory.rs::MemoryReadChunkTool::call` — `capability.can_read` check
/// is between hit-cache resolution and the TOCTOU guard, and there is
/// no hook call there). The registered hook must therefore NEVER see this
/// call. This verifies that capability is the outer boundary and hooks are
/// the inner ring — a denial from the outer ring does not trigger the
/// inner ring's audit trail.
#[tokio::test(flavor = "multi_thread")]
async fn memory_read_chunk_capability_denied_bypasses_hook_pipeline() {
    let harness = build_harness();
    // Mint a hit id DIRECTLY for a path outside the capability's
    // search_scopes. The semantic_search tool would never produce such a
    // hit id legitimately (it capability-gates scope before minting), but
    // the belt-and-braces check inside `memory_read_chunk` exists exactly
    // for this case — see `capability.can_read` inside
    // `MemoryReadChunkTool::call`.
    let out_of_scope_path = MemoryPath::parse("/var/memory/other/stranger.md").unwrap();
    let version = MemoryVersion(1);
    let hit_id = harness.hit_cache.mint(
        harness.tenant.clone(),
        out_of_scope_path.clone(),
        0,
        version,
    );

    let captured = Arc::new(Mutex::new(Vec::new()));
    let pipeline = pipeline_with_hook(Arc::new(RecordingHook {
        name: "should-not-fire".to_string(),
        captured: Arc::clone(&captured),
    }));
    let registry = build_registry(&harness, Some(pipeline));
    let token = capability_token(harness.capability.clone());

    let result = registry
        .call("memory_read_chunk", json!({ "hit_id": hit_id.0 }), &token)
        .await
        .unwrap();

    assert_eq!(
        result,
        json!({
            "error": "hit_not_found",
            "code": 404,
        }),
        "out-of-scope hit must collapse to 404 to avoid leaking grant shape"
    );

    // The hook must not have fired at all — capability is the outer ring,
    // hooks are the inner ring, and the outer ring rejected this call.
    let entries = captured.lock().unwrap();
    assert!(
        entries.is_empty(),
        "hook fired for a capability-denied call (captured={entries:?})"
    );
}

// S037 §13 line 1144: during reindex, semantic_search still works but
// records `memory.search.reindexing=true` on the memory_search span.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_records_reindexing_true_when_backlog_nonempty() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close checklist",
    );

    // Directly stage a backlog row so the reindexing flag reads true.
    // Use the existing `enqueue_backlog_from_content` helper — the path
    // was just written via seed_chunk, so it's present in memory_content.
    let enqueued = harness
        .vector_index
        .enqueue_backlog_from_content(&harness.tenant)
        .unwrap();
    assert!(enqueued >= 1, "seed_chunk must leave a content row");

    let registry = build_registry(&harness, None);
    let token = capability_token(harness.capability.clone());

    let (_, spans, _) = capture_with_subscriber(|| async {
        registry
            .call(
                "semantic_search",
                json!({
                    "query": "quarterly close",
                    "scope": "/var/memory/self",
                }),
                &token,
            )
            .await
            .unwrap()
    })
    .await;

    let span = span_named(&spans, "memory_search");
    assert_eq!(
        span.fields
            .get("memory.search.reindexing")
            .map(String::as_str),
        Some("true"),
        "memory.search.reindexing must be true while backlog is non-empty (span={span:?})"
    );
}

// S037 §13: mirror case — an empty backlog records reindexing=false.
// Together the two tests lock down that the field is recorded
// unconditionally, not only when true.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_search_records_reindexing_false_when_backlog_empty() {
    let harness = build_harness();
    seed_chunk(
        &harness,
        "/var/memory/self/note.md",
        "quarterly close checklist",
    );
    assert_eq!(
        harness.vector_index.backlog_count(&harness.tenant).unwrap(),
        0,
        "fresh harness should have an empty backlog"
    );

    let registry = build_registry(&harness, None);
    let token = capability_token(harness.capability.clone());

    let (_, spans, _) = capture_with_subscriber(|| async {
        registry
            .call(
                "semantic_search",
                json!({
                    "query": "quarterly close",
                    "scope": "/var/memory/self",
                }),
                &token,
            )
            .await
            .unwrap()
    })
    .await;

    let span = span_named(&spans, "memory_search");
    assert_eq!(
        span.fields
            .get("memory.search.reindexing")
            .map(String::as_str),
        Some("false"),
        "memory.search.reindexing must be false when backlog is empty (span={span:?})"
    );
}
