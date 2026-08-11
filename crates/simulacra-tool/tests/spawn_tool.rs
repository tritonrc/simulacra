use serde_json::{Value, json};
use simulacra_tool::{CapabilityToken, ToolRegistry};
use simulacra_types::{Tool, ToolDefinition, ToolError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tracing_subscriber::layer::SubscriberExt;

/// Test-local stand-in used only to verify `ToolRegistry` dispatch and tracing.
/// Production `spawn_agent` schema and result coverage lives in simulacra-runtime.
struct RegistrySpawnStub;

impl Tool for RegistrySpawnStub {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".into(),
            description: "Test-local spawn-shaped registry stub.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "placement": {
                        "type": "string",
                        "description": "Configured child placement"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task or instruction delegated to the child agent"
                    }
                },
                "required": ["placement", "task"],
                "additionalProperties": false
            }),
        }
    }

    fn call(
        &self,
        arguments: Value,
        _capability: &CapabilityToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, ToolError>> + Send + '_>>
    {
        let placement = arguments
            .get("placement")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let child_id = format!(
            "child-{:016x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        Box::pin(async move {
            Ok(json!({
                "child_id": child_id,
                "placement": placement,
                "status": "running"
            }))
        })
    }
}

#[derive(Debug, Clone)]
struct CapturedSpan {
    name: String,
    fields: HashMap<String, String>,
}

struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
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
}

fn capture_spans<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedSpan>) {
    static TRACING_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    static CAPTURED_SPANS: OnceLock<Arc<Mutex<Vec<CapturedSpan>>>> = OnceLock::new();
    static CAPTURE_INSTALL: OnceLock<()> = OnceLock::new();

    let _guard = TRACING_CAPTURE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();

    CAPTURE_INSTALL.get_or_init(|| {
        let spans = Arc::new(Mutex::new(Vec::new()));
        CAPTURED_SPANS
            .set(Arc::clone(&spans))
            .expect("span capture store should only initialize once");
        let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
            spans: Arc::clone(&spans),
        });
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should install");
        tracing::callsite::rebuild_interest_cache();
    });

    let spans = CAPTURED_SPANS
        .get()
        .expect("span capture store should be installed");
    spans.lock().unwrap().clear();
    tracing::callsite::rebuild_interest_cache();
    let result = f();
    tracing::callsite::rebuild_interest_cache();
    let spans = spans.lock().unwrap().clone();
    (result, spans)
}

fn registry_with_spawn_tool() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(RegistrySpawnStub))
        .expect("test tool registration should succeed");
    registry
}

// NOTE: The test for ToolError::ExecutionFailed on failures is in
// crates/simulacra-runtime/tests/s018_subagent_red.rs as
// spawn_agent_tool_child_runtime_failures_return_toolerror_execution_failed,
// which tests the real SpawnAgentTool with an mpsc channel.

// NOTE: auto_approved and restart_strategy tests for the real SpawnAgentTool
// are in crates/simulacra-runtime/tests/s018_subagent_red.rs. The Tool trait has
// no auto_approved() method, so those properties are tested at the runtime
// layer where they are enforced.

#[test]
fn test_local_spawn_shaped_tool_invocation_emits_the_normal_registry_tool_span() {
    let (_, spans) = capture_spans(|| {
        let registry = registry_with_spawn_tool();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(registry.call(
            "spawn_agent",
            json!({
                "placement": "workspace",
                "task": "Investigate"
            }),
            &CapabilityToken::default(),
        ));
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "tool_invoke"
                && span.fields.get("gen_ai.tool.name").map(String::as_str) == Some("spawn_agent")
        }),
        "the registry should use the standard tool invocation span surface"
    );
}
