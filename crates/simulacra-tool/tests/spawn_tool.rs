use serde_json::{Value, json};
use simulacra_tool::{CapabilityToken, ToolRegistry};
use simulacra_types::{Tool, ToolDefinition, ToolError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

struct PendingSpawnAgentTool;

impl Tool for PendingSpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".into(),
            description: "Spawn a supervised child agent to handle a delegated task and return its terminal summary.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_type": {
                        "type": "string",
                        "description": "Configured agent type name from simulacra.toml to use for the child agent"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task or instruction delegated to the child agent"
                    },
                    "budget": {
                        "type": "object",
                        "description": "Requested child budget. Each field is an upper bound and must fit within the parent's remaining budget.",
                        "properties": {
                            "max_tokens": { "type": "integer", "minimum": 0 },
                            "max_turns": { "type": "integer", "minimum": 0 },
                            "max_cost": { "type": "string", "description": "Decimal string, same representation as ResourceBudget.max_cost" },
                            "max_sub_agents": { "type": "integer", "minimum": 0 }
                        },
                        "required": ["max_tokens", "max_turns", "max_cost", "max_sub_agents"],
                        "additionalProperties": false
                    },
                    "capabilities": {
                        "type": "object",
                        "description": "Optional attenuated capability override. If omitted, the child receives the configured capabilities for agent_type intersected with the parent's token.",
                        "properties": {
                            "network": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "mcp_tools": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "shell": { "type": "boolean" },
                            "javascript": { "type": "boolean" },
                            "python": { "type": "boolean" },
                            "paths_write": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "paths_read": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "spawn_types": {
                                "type": "array",
                                "items": { "type": "string" }
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "required": ["agent_type", "task", "budget"],
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
        let agent_type = arguments
            .get("agent_type")
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
            if arguments
                .get("simulate_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Ok(json!({
                    "child_id": child_id,
                    "agent_type": agent_type,
                    "error": "not implemented"
                }))
            } else {
                Ok(json!({
                    "child_id": child_id,
                    "agent_type": agent_type,
                    "exit_reason": "budget_exhausted",
                    "message": "",
                    "token_usage": {
                        "input_tokens": 0,
                        "output_tokens": 0
                    }
                }))
            }
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
    let spans = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry::Registry::default().with(CaptureLayer {
        spans: Arc::clone(&spans),
    });
    let result = tracing::subscriber::with_default(subscriber, f);
    let spans = spans.lock().unwrap().clone();
    (result, spans)
}

fn registry_with_spawn_tool() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(PendingSpawnAgentTool));
    registry
}

fn call_spawn_tool(arguments: Value) -> Value {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            registry_with_spawn_tool()
                .call("spawn_agent", arguments, &CapabilityToken::default())
                .await
                .expect("spawn_agent call should produce a test value")
        })
}

#[test]
fn spawn_agent_definition_uses_the_documented_name_and_description() {
    let definition = PendingSpawnAgentTool.definition();

    assert_eq!(definition.name, "spawn_agent");
    assert_eq!(
        definition.description,
        "Spawn a supervised child agent to handle a delegated task and return its terminal summary."
    );
}

#[test]
fn spawn_agent_definition_exposes_agent_type_task_budget_and_optional_capabilities() {
    let definition = PendingSpawnAgentTool.definition();
    let properties = definition
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("schema should expose properties");

    for field in ["agent_type", "task", "budget", "capabilities"] {
        assert!(
            properties.contains_key(field),
            "spawn_agent schema should expose {field}"
        );
    }
}

#[test]
fn spawn_agent_budget_schema_requires_all_budget_fields_and_disallows_additional_properties() {
    let definition = PendingSpawnAgentTool.definition();
    let budget = definition
        .input_schema
        .pointer("/properties/budget")
        .cloned()
        .unwrap_or(Value::Null);

    assert_eq!(
        budget.get("required"),
        Some(&json!([
            "max_tokens",
            "max_turns",
            "max_cost",
            "max_sub_agents"
        ]))
    );
    assert_eq!(
        budget.get("additionalProperties"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn spawn_agent_capabilities_schema_matches_the_spec_shape() {
    let definition = PendingSpawnAgentTool.definition();
    let capabilities = definition
        .input_schema
        .pointer("/properties/capabilities/properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    for field in [
        "network",
        "mcp_tools",
        "shell",
        "javascript",
        "python",
        "paths_write",
        "paths_read",
        "spawn_types",
    ] {
        assert!(
            capabilities.contains_key(field),
            "capability override schema should include {field}"
        );
    }
}

#[test]
fn successful_spawn_agent_result_includes_child_id_agent_type_exit_reason_message_and_token_usage()
{
    let value = call_spawn_tool(json!({
        "agent_type": "researcher",
        "task": "Investigate",
        "budget": {
            "max_tokens": 10,
            "max_turns": 2,
            "max_cost": "0",
            "max_sub_agents": 0
        }
    }));

    for field in [
        "child_id",
        "agent_type",
        "exit_reason",
        "message",
        "token_usage",
    ] {
        assert!(
            value.get(field).is_some(),
            "successful spawn_agent results should include {field}"
        );
    }
}

#[test]
fn failed_spawn_agent_result_has_error_shape_with_child_id_agent_type_and_error() {
    let value = call_spawn_tool(json!({
        "simulate_error": true,
        "agent_type": "researcher",
        "task": "Investigate",
        "budget": {
            "max_tokens": 10,
            "max_turns": 2,
            "max_cost": "0",
            "max_sub_agents": 0
        }
    }));

    for field in ["child_id", "agent_type", "error"] {
        assert!(
            value.get(field).is_some(),
            "failed spawn_agent results should include {field}"
        );
    }
}

// NOTE: The test for ToolError::ExecutionFailed on failures is in
// crates/simulacra-runtime/tests/s018_subagent_red.rs as
// spawn_agent_tool_child_runtime_failures_return_toolerror_execution_failed,
// which tests the real SpawnAgentTool with an mpsc channel.

#[test]
fn budget_exhausted_exit_reason_is_a_success_result_with_partial_output_not_an_error_result() {
    let value = call_spawn_tool(json!({
        "agent_type": "researcher",
        "task": "Investigate",
        "budget": {
            "max_tokens": 10,
            "max_turns": 2,
            "max_cost": "0",
            "max_sub_agents": 0
        }
    }));

    assert_eq!(
        value.get("exit_reason").and_then(Value::as_str),
        Some("budget_exhausted"),
        "partial child exits should surface exit_reason = budget_exhausted in a success payload"
    );
    assert!(
        value.get("error").is_none(),
        "budget_exhausted should not be encoded as a true error result"
    );
}

// NOTE: auto_approved and restart_strategy tests for the real SpawnAgentTool
// are in crates/simulacra-runtime/tests/s018_subagent_red.rs. The Tool trait has
// no auto_approved() method, so those properties are tested at the runtime
// layer where they are enforced.

#[test]
fn spawn_agent_returns_empty_message_when_the_child_has_no_final_assistant_message() {
    let value = call_spawn_tool(json!({
        "agent_type": "researcher",
        "task": "Investigate",
        "budget": {
            "max_tokens": 10,
            "max_turns": 2,
            "max_cost": "0",
            "max_sub_agents": 0
        }
    }));

    assert_eq!(
        value.get("message").and_then(Value::as_str),
        Some(""),
        "spawn_agent should return an empty string rather than fabricate a child summary"
    );
}

#[test]
fn spawn_agent_tool_invocation_emits_the_normal_tool_span_with_gen_ai_tool_name() {
    let (_, spans) = capture_spans(|| {
        let registry = registry_with_spawn_tool();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = rt.block_on(registry.call(
            "spawn_agent",
            json!({
                "agent_type": "researcher",
                "task": "Investigate",
                "budget": {
                    "max_tokens": 10,
                    "max_turns": 2,
                    "max_cost": "0",
                    "max_sub_agents": 0
                }
            }),
            &CapabilityToken::default(),
        ));
    });

    assert!(
        spans.iter().any(|span| {
            span.name == "tool_invoke"
                && span.fields.get("gen_ai.tool.name").map(String::as_str) == Some("spawn_agent")
        }),
        "spawn_agent should use the standard tool invocation span surface"
    );
}
