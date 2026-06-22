//! Tests for EventSource trait and ProtocolAdapter trait (S031, S032 assertions).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{Router, routing::get};
use serde_json::json;
use simulacra_server::{
    EventCallback, EventMessage, EventSource, EventSourceError, NativeCommand, NativeEvent,
    ProtocolAdapter, ProtocolError, ProtocolRequest, ProtocolResponse, SimulacraEngine,
};
use tokio_util::sync::CancellationToken;

// ─── Stub EventSource ─────────────────────────────────────────────────────────

struct ImmediatelyCancelledSource;

#[async_trait]
impl EventSource for ImmediatelyCancelledSource {
    fn source_type(&self) -> &str {
        "stub"
    }

    async fn start(
        &self,
        _config: serde_json::Value,
        _callback: Box<EventCallback>,
        cancel: CancellationToken,
    ) -> Result<(), EventSourceError> {
        // Wait for cancellation, then return Ok.
        cancel.cancelled().await;
        Ok(())
    }
}

// ─── Stub ProtocolAdapter ─────────────────────────────────────────────────────

struct EchoProtocolAdapter;

#[async_trait]
impl ProtocolAdapter for EchoProtocolAdapter {
    fn protocol_id(&self) -> &str {
        "echo"
    }

    fn routes(&self, _engine: Arc<SimulacraEngine>) -> Router {
        Router::new().route("/echo/ping", get(|| async { "pong" }))
    }

    async fn translate_inbound(
        &self,
        request: ProtocolRequest,
    ) -> Result<NativeCommand, ProtocolError> {
        Ok(NativeCommand {
            name: "echo".to_string(),
            payload: request.body,
        })
    }

    async fn translate_outbound(
        &self,
        event: NativeEvent,
    ) -> Result<Option<ProtocolResponse>, ProtocolError> {
        Ok(Some(ProtocolResponse {
            status: 200,
            body: json!({"echoed": event.event_type}),
        }))
    }
}

// ─── EventSource trait assertions ────────────────────────────────────────────

#[test]
fn event_source_trait_compiles_and_is_object_safe_send_sync() {
    // The trait must be usable as a trait object (object-safe).
    let _source: Arc<dyn EventSource> = Arc::new(ImmediatelyCancelledSource);
    let _source_box: Box<dyn EventSource> = Box::new(ImmediatelyCancelledSource);
    // If this compiles, the trait is object-safe with Send + Sync.
}

#[test]
fn event_message_struct_contains_source_type_source_id_payload_metadata_and_timestamp() {
    let message = EventMessage {
        source_type: "kafka".to_string(),
        source_id: "orders-topic/partition-0".to_string(),
        payload: json!({"order_id": 42, "status": "shipped"}),
        metadata: HashMap::from([
            ("partition".to_string(), "0".to_string()),
            ("offset".to_string(), "12345".to_string()),
        ]),
        timestamp: chrono::Utc::now(),
    };

    assert_eq!(message.source_type, "kafka");
    assert_eq!(message.source_id, "orders-topic/partition-0");
    assert_eq!(message.payload["order_id"], json!(42));
    assert_eq!(
        message.metadata.get("partition").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        message.metadata.get("offset").map(String::as_str),
        Some("12345")
    );
    // timestamp must be present (non-zero)
    assert!(message.timestamp.timestamp() > 0);
}

#[tokio::test]
async fn event_source_start_returns_ok_when_cancellation_token_is_triggered() {
    let source = ImmediatelyCancelledSource;
    let cancel = CancellationToken::new();

    // Trigger cancellation before calling start.
    cancel.cancel();

    let callback: Box<EventCallback> = Box::new(|_msg: EventMessage| Box::pin(async move {}));

    let result = source.start(json!({}), callback, cancel).await;

    assert!(
        result.is_ok(),
        "event source must return Ok(()) on clean cancellation, got: {:?}",
        result
    );
}

#[tokio::test]
async fn event_source_respects_cancellation_token_for_graceful_shutdown() {
    let source = ImmediatelyCancelledSource;
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    // Cancel after a short delay.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        cancel_clone.cancel();
    });

    let callback: Box<EventCallback> = Box::new(|_msg: EventMessage| Box::pin(async move {}));

    let result = source.start(json!({}), callback, cancel).await;
    assert!(
        result.is_ok(),
        "event source must shut down cleanly on cancellation"
    );
}

// ─── ProtocolAdapter trait assertions ─────────────────────────────────────────

#[test]
fn protocol_adapter_trait_compiles_and_is_object_safe() {
    let _adapter: Box<dyn ProtocolAdapter> = Box::new(EchoProtocolAdapter);
    let _adapter_arc: Arc<dyn ProtocolAdapter> = Arc::new(EchoProtocolAdapter);
}

#[tokio::test]
async fn protocol_adapter_routes_mounts_custom_routes() {
    let adapter = EchoProtocolAdapter;
    use std::collections::HashMap;
    let config = simulacra_config::SimulacraConfig {
        project: simulacra_config::ProjectConfig {
            name: "test".into(),
            description: None,
        },
        agent_types: HashMap::new(),
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: simulacra_config::VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: simulacra_config::CatalogConfig::default(),
    };
    let engine = Arc::new(
        SimulacraEngine::new_with_in_memory_catalog(config, None)
            .await
            .unwrap(),
    );
    let _router: Router = adapter.routes(engine);
    // Structural assertion: routes() returns a valid Router.
    // (Route reachability tested in integration tests.)
}

#[tokio::test]
async fn adapters_translate_inbound_requests_to_native_commands() {
    let adapter = EchoProtocolAdapter;
    let request = ProtocolRequest {
        path: "/echo/tasks".to_string(),
        body: json!({"command": "task.create", "task": "draft report"}),
    };

    let result = adapter.translate_inbound(request).await;

    assert!(
        result.is_ok(),
        "echo adapter must translate inbound requests"
    );
    let cmd = result.unwrap();
    assert_eq!(cmd.name, "echo");
    assert_eq!(cmd.payload["command"], json!("task.create"));
}

#[tokio::test]
async fn adapters_translate_native_events_to_protocol_specific_responses() {
    let adapter = EchoProtocolAdapter;
    let event = NativeEvent {
        event_type: "task.state_changed".to_string(),
        payload: json!({"task_id": "task-1", "to": "running"}),
    };

    let result = adapter.translate_outbound(event).await;

    assert!(result.is_ok(), "echo adapter must translate native events");
    let response = result.unwrap();
    assert!(response.is_some(), "echo adapter must produce a response");
    let response = response.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body["echoed"], json!("task.state_changed"));
}

#[test]
fn protocol_adapter_protocol_id_returns_unique_identifier() {
    let adapter = EchoProtocolAdapter;
    assert_eq!(adapter.protocol_id(), "echo");
}
