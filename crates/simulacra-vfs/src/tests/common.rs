use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use simulacra_types::{VfsError, VirtualFs};
use tracing_subscriber::layer::SubscriberExt;

use crate::MemoryFs;

#[derive(Clone)]
pub(super) struct SharedFs {
    inner: Arc<dyn VirtualFs>,
}

impl SharedFs {
    pub(super) fn memory() -> Self {
        Self {
            inner: Arc::new(MemoryFs::new()),
        }
    }
}

impl VirtualFs for SharedFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<simulacra_types::FsMetadata, VfsError> {
        self.inner.metadata(path)
    }

    fn snapshot(&self) -> Result<simulacra_types::VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }

    fn restore(&self, snapshot: &simulacra_types::VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

#[derive(Debug, Clone)]
pub(super) struct CapturedSpan {
    pub(super) name: String,
    pub(super) fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub(super) struct CapturedEvent {
    #[allow(dead_code)]
    pub(super) name: String,
    pub(super) level: String,
    pub(super) fields: HashMap<String, String>,
    pub(super) current_span: Option<String>,
}

struct SpanCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl<S> tracing_subscriber::Layer<S> for SpanCaptureLayer
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
                    for (key, value) in new_fields {
                        captured.fields.insert(key, value);
                    }
                    break;
                }
            }
        }
    }
}

struct TraceCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for TraceCaptureLayer
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

        let current_span = ctx
            .current_span()
            .id()
            .and_then(|id| ctx.span(id))
            .map(|span| span.name().to_string());

        self.events.lock().unwrap().push(CapturedEvent {
            name: event.metadata().name().to_string(),
            level: event.metadata().level().as_str().to_string(),
            fields,
            current_span,
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

fn setup_span_capture() -> (
    impl tracing::Subscriber + Send + Sync,
    Arc<Mutex<Vec<CapturedSpan>>>,
) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let layer = SpanCaptureLayer {
        spans: Arc::clone(&spans),
    };
    let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
    (subscriber, spans)
}

pub(super) fn capture_spans<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>) {
    let (subscriber, captured) = setup_span_capture();
    let result = tracing::subscriber::with_default(subscriber, f);
    let spans = captured.lock().unwrap().clone();
    (result, spans)
}

pub(super) fn capture_trace<T>(
    f: impl FnOnce() -> T,
) -> (T, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let layer = TraceCaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    };
    let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
    let result = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

pub(super) fn field_matches(span: &CapturedSpan, key: &str, expected: &str) -> bool {
    span.fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

pub(super) fn event_field_matches(event: &CapturedEvent, key: &str, expected: &str) -> bool {
    event
        .fields
        .get(key)
        .map(|value| value.trim_matches('"') == expected)
        .unwrap_or(false)
}

pub(super) fn assert_span_with_path(spans: &[CapturedSpan], operation: &str, path: &str) {
    let span = spans
        .iter()
        .find(|span| {
            field_matches(span, "simulacra.operation.name", operation)
                && field_matches(span, "simulacra.vfs.path", path)
        })
        .unwrap_or_else(|| {
            panic!("expected span for operation {operation} and path {path}; got {spans:#?}")
        });

    assert!(
        span.name.contains(operation),
        "span name should contain {operation}, got {}",
        span.name
    );
}

pub(super) fn assert_span(spans: &[CapturedSpan], operation: &str) {
    let span = spans
        .iter()
        .find(|span| field_matches(span, "simulacra.operation.name", operation))
        .unwrap_or_else(|| panic!("expected span for operation {operation}; got {spans:#?}"));

    assert!(
        span.name.contains(operation),
        "span name should contain {operation}, got {}",
        span.name
    );
}
