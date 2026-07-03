    use super::*;
    use std::sync::{Mutex as StdMutex, OnceLock};
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Debug, Clone)]
    struct CapturedSpan {
        name: String,
        fields: std::collections::HashMap<String, String>,
    }

    #[derive(Debug, Clone)]
    struct CapturedEvent {
        #[allow(dead_code)]
        name: String,
        level: String,
        current_span: Option<String>,
        fields: std::collections::HashMap<String, String>,
    }

    struct SpanCaptureLayer {
        spans: Arc<StdMutex<Vec<CapturedSpan>>>,
        events: Arc<StdMutex<Vec<CapturedEvent>>>,
    }

    impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>
        tracing_subscriber::Layer<S> for SpanCaptureLayer
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = std::collections::HashMap::new();
            let mut visitor = FieldVisitor(&mut fields);
            attrs.record(&mut visitor);
            let span = CapturedSpan {
                name: attrs.metadata().name().to_string(),
                fields,
            };
            self.spans.lock().unwrap().push(span);
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let span_ref = ctx.span(id);
            if let Some(span_ref) = span_ref {
                let span_name = span_ref.name().to_string();
                let mut new_fields = std::collections::HashMap::new();
                let mut visitor = FieldVisitor(&mut new_fields);
                values.record(&mut visitor);
                let mut spans = self.spans.lock().unwrap();
                for captured in spans.iter_mut().rev() {
                    if captured.name == span_name {
                        for (k, v) in new_fields {
                            captured.fields.insert(k, v);
                        }
                        break;
                    }
                }
            }
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = std::collections::HashMap::new();
            let mut visitor = FieldVisitor(&mut fields);
            event.record(&mut visitor);
            let captured = CapturedEvent {
                name: event.metadata().name().to_string(),
                level: event.metadata().level().to_string(),
                current_span: ctx.lookup_current().map(|span| span.name().to_string()),
                fields,
            };
            self.events.lock().unwrap().push(captured);
        }
    }

    struct FieldVisitor<'a>(&'a mut std::collections::HashMap<String, String>);

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
        fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    #[allow(clippy::type_complexity)]
    fn setup_capture() -> (
        impl tracing::Subscriber + Send + Sync,
        Arc<StdMutex<Vec<CapturedSpan>>>,
        Arc<StdMutex<Vec<CapturedEvent>>>,
    ) {
        let spans = Arc::new(StdMutex::new(Vec::new()));
        let events = Arc::new(StdMutex::new(Vec::new()));
        let layer = SpanCaptureLayer {
            spans: Arc::clone(&spans),
            events: Arc::clone(&events),
        };
        let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
        (subscriber, spans, events)
    }

    async fn install_capture<S>(
        subscriber: S,
    ) -> (
        tokio::sync::MutexGuard<'static, ()>,
        tracing::dispatcher::DefaultGuard,
    )
    where
        S: tracing::Subscriber + Send + Sync + 'static,
    {
        static TRACING_CAPTURE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        let capture_guard = TRACING_CAPTURE_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let default_guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        (capture_guard, default_guard)
    }

    // S010 Assertion: Agent spans use invoke_agent operation name
