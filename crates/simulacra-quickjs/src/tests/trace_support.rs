//! Red tests for `specs/S003-quickjs.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub(crate) struct CapturedSpan {
    pub(crate) name: String,
    pub(crate) fields: HashMap<String, String>,
    pub(crate) parent: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct CapturedEvent {
    pub(crate) name: String,
    pub(crate) level: String,
    pub(crate) fields: HashMap<String, String>,
    pub(crate) current_span: Option<String>,
}

pub(crate) struct TraceCaptureLayer {
    pub(crate) spans: Arc<Mutex<Vec<CapturedSpan>>>,
    pub(crate) events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for TraceCaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = HashMap::new();
        let mut visitor = FieldVisitor(&mut fields);
        attrs.record(&mut visitor);

        let parent = attrs
            .parent()
            .and_then(|parent_id| ctx.span(parent_id))
            .map(|span| span.name().to_string())
            .or_else(|| {
                if attrs.is_contextual() {
                    ctx.current_span()
                        .id()
                        .and_then(|parent_id| ctx.span(parent_id))
                        .map(|span| span.name().to_string())
                } else {
                    None
                }
            });

        self.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields,
            parent,
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
