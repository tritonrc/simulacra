use super::*;

#[derive(Debug, Clone)]
pub struct CapturedSpan {
    pub fields: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct CapturedEvent {
    pub level: String,
    pub current_span: Option<String>,
    pub fields: HashMap<String, String>,
}

pub struct CaptureLayer {
    pub spans: Arc<Mutex<Vec<CapturedSpan>>>,
    pub span_fields: Arc<Mutex<HashMap<tracing::span::Id, HashMap<String, String>>>>,
    pub events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);
        self.span_fields
            .lock()
            .unwrap()
            .insert(id.clone(), fields.clone());
        self.spans.lock().unwrap().push(CapturedSpan { fields });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut new_fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut new_fields);
        values.record(&mut visitor);

        let mut span_fields = self.span_fields.lock().unwrap();
        if let Some(existing) = span_fields.get_mut(id) {
            existing.extend(new_fields.clone());
        }

        // Update the matching span in the captured list
        let mut spans = self.spans.lock().unwrap();
        if let Some(sf) = span_fields.get(id) {
            // Find the span whose fields match the original (before record)
            // and update it with the new fields
            for span in spans.iter_mut().rev() {
                // Match by checking the span has the same base fields
                let is_match = sf
                    .iter()
                    .filter(|(k, _)| !new_fields.contains_key(k.as_str()))
                    .all(|(k, v)| span.fields.get(k) == Some(v));
                if is_match {
                    span.fields.extend(new_fields);
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

pub struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

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

#[allow(clippy::type_complexity)]
pub fn setup_capture() -> (
    impl tracing::Subscriber + Send + Sync,
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        span_fields: Arc::new(Mutex::new(HashMap::new())),
        events: Arc::clone(&events),
    });
    (subscriber, spans, events)
}

pub fn capture_operation<R>(
    operation: impl FnOnce() -> R,
) -> (R, Vec<CapturedSpan>, Vec<CapturedEvent>) {
    static GLOBAL_TRACING: OnceLock<()> = OnceLock::new();
    GLOBAL_TRACING.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
        tracing::callsite::rebuild_interest_cache();
    });

    static CAPTURE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = CAPTURE_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("capture mutex should not be poisoned");
    let (subscriber, spans, events) = setup_capture();
    let result = tracing::subscriber::with_default(subscriber, operation);
    let spans = spans.lock().unwrap().clone();
    let events = events.lock().unwrap().clone();
    (result, spans, events)
}

/// Run `operation` under a capturing subscriber and return the recorded spans
/// (events are discarded). A convenience wrapper around [`capture_operation`].
pub fn capture_spans<R>(operation: impl FnOnce() -> R) -> (R, Vec<CapturedSpan>) {
    let (result, spans, _events) = capture_operation(operation);
    (result, spans)
}

/// Collect the sorted `simulacra.operation.name` field values from `spans`.
pub fn span_operations(spans: &[CapturedSpan]) -> Vec<String> {
    let mut operations = spans
        .iter()
        .filter_map(|span| span.fields.get("simulacra.operation.name").cloned())
        .collect::<Vec<_>>();
    operations.sort();
    operations
}
