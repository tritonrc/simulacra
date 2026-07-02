use rust_decimal::Decimal;
use serde_json::{Value, json};
use simulacra_sandbox::AgentCell;
use simulacra_tool::{ToolError, ToolRegistry, register_builtins};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JOURNAL_SCHEMA_VERSION, JournalEntry,
    JournalEntryKind, JournalError, JournalStorage, PathPattern, ResourceBudget, TokenUsage,
    VirtualFs,
};
use simulacra_vfs::MemoryFs;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::MutexGuard;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug, Default)]
struct FakeJournalStorage {
    entries: Mutex<Vec<JournalEntry>>,
}

impl JournalStorage for FakeJournalStorage {
    fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
        self.entries.lock().unwrap().push(entry);
        Ok(())
    }

    fn read_all(&self, agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.agent_id == *agent_id)
            .cloned()
            .collect())
    }

    fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
        Ok(TokenUsage::default())
    }

    fn save_checkpoint(
        &self,
        agent_id: &AgentId,
        _after_entry: usize,
        data: CheckpointData,
    ) -> Result<(), JournalError> {
        let snapshot_data =
            serde_json::to_vec(&data).map_err(|error| JournalError::Storage(error.to_string()))?;
        self.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::Checkpoint { snapshot_data },
        })
    }

    fn fork_from(
        &self,
        agent_id: &AgentId,
        checkpoint_idx: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if checkpoint_idx >= entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(checkpoint_idx));
        }
        Ok(entries[..=checkpoint_idx].to_vec())
    }

    fn read_from(
        &self,
        agent_id: &AgentId,
        start_index: usize,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        let entries = self.read_all(agent_id)?;
        if start_index > entries.len() {
            return Err(JournalError::InvalidCheckpointIndex(start_index));
        }
        Ok(entries[start_index..].to_vec())
    }
}

struct Harness {
    registry: ToolRegistry,
    vfs: Arc<MemoryFs>,
    journal: Arc<FakeJournalStorage>,
}

impl Harness {
    fn new(capability: CapabilityToken, budget: ResourceBudget) -> Self {
        let vfs = Arc::new(MemoryFs::new());
        let vfs_dyn: Arc<dyn VirtualFs> = vfs.clone();
        let journal = Arc::new(FakeJournalStorage::default());
        let journal_dyn: Arc<dyn JournalStorage> = journal.clone();
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        let cell = Arc::new(AgentCell::new(
            vfs_dyn,
            capability,
            Arc::new(Mutex::new(budget)),
            journal_dyn,
            http_client,
        ));
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry, Arc::clone(&cell))
            .expect("built-in registration should succeed");

        let _ = cell;

        Self {
            registry,
            vfs,
            journal,
        }
    }
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    parent_name: Option<String>,
    fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: String,
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
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);

        let parent_name = attrs
            .parent()
            .and_then(|parent| ctx.span(parent).map(|span| span.name().to_string()))
            .or_else(|| {
                ctx.span(id)
                    .and_then(|span| span.parent().map(|parent| parent.name().to_string()))
            })
            .or_else(|| ctx.lookup_current().map(|span| span.name().to_string()));

        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            parent_name,
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
                    for (key, value) in new_fields {
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
            level: event.metadata().level().to_string(),
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

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn registry_call_guard() -> MutexGuard<'static, ()> {
    static REGISTRY_CALL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    REGISTRY_CALL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap()
}

fn capture_async<R>(operation: impl FnOnce() -> R) -> (R, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    static TRACING_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = TRACING_CAPTURE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });
    let result = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let result = operation();
        tracing::callsite::rebuild_interest_cache();
        result
    });
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

fn full_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![PathPattern("/**".into())],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

fn no_read_capability() -> CapabilityToken {
    CapabilityToken {
        shell: true,
        javascript: true,
        paths_read: vec![],
        paths_write: vec![PathPattern("/**".into())],
        ..Default::default()
    }
}

fn unlimited_budget() -> ResourceBudget {
    ResourceBudget::new(0, 0, Decimal::ZERO, 0)
}

fn budget_with_vfs_bytes_exhausted() -> ResourceBudget {
    ResourceBudget {
        max_vfs_bytes: 1,
        used_vfs_bytes: 1,
        ..ResourceBudget::new(0, 0, Decimal::ZERO, 0)
    }
}

fn call_tool(
    harness: &Harness,
    name: &str,
    arguments: Value,
    capability: &CapabilityToken,
) -> Result<Value, ToolError> {
    call_registry(&harness.registry, name, arguments, capability)
}

fn call_registry(
    registry: &ToolRegistry,
    name: &str,
    arguments: Value,
    capability: &CapabilityToken,
) -> Result<Value, ToolError> {
    let _guard = registry_call_guard();
    tracing::callsite::rebuild_interest_cache();
    run_async(registry.call(name, arguments, capability))
}

fn string_result(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn tool_content(value: &Value) -> String {
    value
        .get("content")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| string_result(value))
}

fn tool_structured(value: &Value) -> &Value {
    value.get("structured").unwrap_or(value)
}

fn assert_error_result_contains(value: &Value, expected_substring: &str) {
    assert_eq!(
        value.get("is_error").and_then(Value::as_bool),
        Some(true),
        "expected an error-shaped tool result, got {value:?}"
    );

    let rendered = value.to_string().to_ascii_lowercase();
    assert!(
        rendered.contains(&expected_substring.to_ascii_lowercase()),
        "expected {value:?} to mention {expected_substring:?}"
    );
}

fn assert_invalid_arguments(result: Result<Value, ToolError>) {
    match result {
        Err(ToolError::InvalidArguments(_)) => {}
        other => panic!("expected invalid arguments error, got {other:?}"),
    }
}
