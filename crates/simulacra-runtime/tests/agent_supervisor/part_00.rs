use rust_decimal::Decimal;
use simulacra_runtime::{
    AgentLoop, AgentLoopConfig, AgentLoopOutput, AgentSupervisor, BoxTaskFuture, CancellationToken,
    InMemoryJournalStorage, MessagePriority, RestartStrategy, RuntimeError, SpawnConfig,
    SupervisorMessage, SupervisorPayload, TaskFactory,
};
use simulacra_tool::ToolRegistry;
use simulacra_types::{
    AgentId, CapabilityToken, ContextStrategy, ExitReason, FinishReason, JournalStorage, Message,
    Provider, ProviderError, ProviderResponse, ResourceBudget, Role, TokenUsage, Tool,
    ToolCallMessage, ToolDefinition, ToolError,
};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tracing_subscriber::layer::SubscriberExt;

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

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

#[allow(clippy::type_complexity)]
fn setup_capture() -> (
    impl tracing::Subscriber + Send + Sync,
    Arc<Mutex<Vec<CapturedSpan>>>,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
        events: Arc::clone(&events),
    });
    (subscriber, spans, events)
}

struct FakeProvider {
    responses: Mutex<Vec<ProviderResponse>>,
}

impl FakeProvider {
    fn new(responses: Vec<ProviderResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl Provider for FakeProvider {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProviderResponse, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut responses = self
                .responses
                .lock()
                .map_err(|err| ProviderError::Other(format!("lock poisoned: {err}")))?;

            if responses.is_empty() {
                return Err(ProviderError::Other(
                    "FakeProvider: no more canned responses".into(),
                ));
            }

            Ok(responses.remove(0))
        })
    }
}

struct PassthroughContext;

impl ContextStrategy for PassthroughContext {
    fn compact(&self, messages: &[Message], _token_limit: u64) -> Vec<Message> {
        messages.to_vec()
    }
}

struct EchoTool;

impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Echoes input".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn call(
        &self,
        arguments: serde_json::Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(arguments) })
    }
}

fn text_response(content: &str) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        finish_reason: FinishReason::EndTurn,
        provider_response_id: Some("resp-1".into()),
        model: "test-model".into(),
    }
}

fn tool_call_response(tool_name: &str, arguments: serde_json::Value) -> ProviderResponse {
    ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCallMessage {
                id: "tc-1".into(),
                name: tool_name.into(),
                arguments,
            }],
            tool_call_id: None,
        },
        token_usage: TokenUsage {
            input_tokens: 20,
            output_tokens: 10,
        },
        finish_reason: FinishReason::ToolUse,
        provider_response_id: Some("resp-2".into()),
        model: "test-model".into(),
    }
}

