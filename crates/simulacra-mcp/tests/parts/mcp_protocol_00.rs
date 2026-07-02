use serde_json::json;
use simulacra_mcp::{McpError, McpManager};
use simulacra_types::{
    AgentId, CapabilityToken, CheckpointData, JournalEntry, JournalEntryKind, JournalError,
    JournalStorage, TokenUsage,
};
use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing_subscriber::layer::SubscriberExt;

fn capability_with_mcp_tools(patterns: &[&str]) -> CapabilityToken {
    CapabilityToken {
        mcp_tools: patterns
            .iter()
            .map(|pattern| (*pattern).to_string())
            .collect(),
        ..Default::default()
    }
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
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
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);

        self.spans
            .lock()
            .expect("span capture mutex should not be poisoned")
            .push(CapturedSpan {
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

            let mut spans = self
                .spans
                .lock()
                .expect("span capture mutex should not be poisoned");
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

        self.events
            .lock()
            .expect("event capture mutex should not be poisoned")
            .push(CapturedEvent {
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

static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
static CAPTURED_EVENTS: OnceLock<Arc<Mutex<Vec<CapturedEvent>>>> = OnceLock::new();
static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();
static TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn capture_store() -> (
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));

        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("span capture store should only initialize once");
        CAPTURED_EVENTS
            .set(Arc::clone(&events))
            .expect("event capture store should only initialize once");

        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer { spans, events });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
        tracing::callsite::rebuild_interest_cache();
    });

    (
        Arc::clone(
            CAPTURED_SPANS
                .get()
                .expect("span capture store should be installed"),
        ),
        Arc::clone(
            CAPTURED_EVENTS
                .get()
                .expect("event capture store should be installed"),
        ),
    )
}

fn blocking_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    let _ = capture_store();
    TEST_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .blocking_lock()
}

async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    let _ = capture_store();
    TEST_MUTEX
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn capture_traces<T>(operation: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    let _guard = blocking_test_guard();
    tracing::callsite::rebuild_interest_cache();
    let (spans, events) = capture_store();
    spans
        .lock()
        .expect("span capture mutex should not be poisoned")
        .clear();
    events
        .lock()
        .expect("event capture mutex should not be poisoned")
        .clear();

    let result = operation();
    tracing::callsite::rebuild_interest_cache();
    let captured_spans = spans
        .lock()
        .expect("span capture mutex should not be poisoned")
        .clone();
    let captured_events = events
        .lock()
        .expect("event capture mutex should not be poisoned")
        .clone();
    (result, captured_spans, captured_events)
}

fn field_matches(fields: &HashMap<String, String>, key: &str, expected: &str) -> bool {
    fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

fn run_async<F>(future: F) -> F::Output
where
    F: Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build")
        .block_on(future)
}
