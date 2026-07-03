//! Simulacra API server — HTTP/WebSocket interface over the Simulacra engine.
//!
//! Provides:
//! - WebSocket bidirectional transport (primary)
//! - REST + SSE fallback transport
//! - OIDC and API key authentication
//! - Namespace-based multi-tenancy
//! - Task lifecycle state machine
//! - Webhook receivers with HMAC-SHA256 validation
//! - Cron/schedule-based task triggering
//! - Pluggable `ProtocolAdapter` trait (A2A, AG-UI)
//! - Pluggable `EventSource` trait (Kafka, SQS, etc.)

pub mod artifact_store;
pub mod auth;
pub mod engine;
pub mod error;
pub mod metrics;
pub mod pool;
pub mod scheduler;
pub mod server;
pub mod task;
pub mod tenant;
pub mod tool_catalog;
pub mod webhook;

pub use artifact_store::{LocalDiskArtifactStore, S3ArtifactStore};
pub use auth::{
    ApiKeyAuthProvider, ApiKeyEntry, AuthError, AuthProvider, CompositeAuthProvider, Credentials,
    Identity, NoAuthProvider, OidcAuthProvider, OidcConfig,
};
pub use engine::{
    EngineActivitySink, EngineError, ProviderFactory, ProviderKind, SimulacraEngine,
    build_provider, infer_provider_kind, map_exit_reason,
};
pub use error::{ApiError, ServerError};
pub use pool::{AgentWorkerPool, WorkerPoolConfig};
pub use scheduler::{MissedPolicy, ScheduleConfig, ScheduleEntry, Scheduler};
pub use server::{
    AppState, FileAttachment, GraphQLMount, ServerConfig, SimulacraServer, build_router,
    start_server, validate_attachments,
};
pub use task::{TaskEventChannel, TaskHandle, TaskManager, TaskManagerError, TaskState};
pub use tenant::{BudgetPoolConfig, TenantConfig, TenantError, TenantResolver};
pub use tool_catalog::{BuiltinToolCatalog, DefaultToolCatalog};
pub use webhook::{WebhookConfig, WebhookHandler, apply_payload_template, compute_hmac_signature};

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

// ──────────────────────────────────────────────────────────────────────────────
// Response envelope
// ──────────────────────────────────────────────────────────────────────────────

/// Consistent JSON response envelope for REST endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiErrorPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorPayload {
    pub code: String,
    pub message: String,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }
}

impl ApiResponse<()> {
    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(ApiErrorPayload {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// API schema
// ──────────────────────────────────────────────────────────────────────────────

/// Returns the full API schema for GET /api/v1/schema.
pub fn api_schema() -> Value {
    serde_json::json!({
        "ok": true,
        "data": {
            "commands": [
                {
                    "name": "task.create",
                    "description": "Create and start a new task",
                    "payload": {
                        "task": "string",
                        "tenant": "string?",
                        "agent_type": "string?",
                        "metadata": "object?",
                        "files": "map<string, {data: string, encoding?: string}>?"
                    }
                },
                {
                    "name": "task.cancel",
                    "description": "Cancel a running task",
                    "payload": {"task_id": "string"}
                },
                {
                    "name": "task.pause",
                    "description": "Pause a running task",
                    "payload": {"task_id": "string"}
                },
                {
                    "name": "task.resume",
                    "description": "Resume a paused task",
                    "payload": {"task_id": "string"}
                },
                {
                    "name": "input.response",
                    "description": "Respond to an input.required event",
                    "payload": {"task_id": "string", "content": "string"}
                },
                {
                    "name": "approval.respond",
                    "description": "Respond to a tool.approval_required event",
                    "payload": {
                        "task_id": "string",
                        "tool_call_id": "string",
                        "approved": "bool",
                        "reason": "string?"
                    }
                },
                {
                    "name": "workflow.start",
                    "description": "Start a workflow run",
                    "payload": {
                        "script": "string?",
                        "name": "string?",
                        "script_path": "string?",
                        "args": "object?",
                        "resume_from_run_id": "string?"
                    }
                }
            ],
            "rest_endpoints": [
                {
                    "method": "GET",
                    "path": "/api/v1/tasks/{task_id}/artifacts",
                    "description": "List all artifacts produced by a task"
                },
                {
                    "method": "GET",
                    "path": "/api/v1/tasks/{task_id}/artifacts/{path}",
                    "description": "Download a single artifact (raw bytes)"
                },
                {
                    "method": "GET",
                    "path": "/api/v1/triggers",
                    "description": "List webhook + schedule triggers for the caller's tenant (optional ?agent= filter)"
                },
                {
                    "method": "POST",
                    "path": "/api/v1/workflows/start",
                    "description": "Start a workflow run"
                },
                {
                    "method": "GET",
                    "path": "/api/v1/workflows/{run_id}/events",
                    "description": "Replay workflow events as SSE"
                }
            ],
            "events": [
                {"name": "task.state_changed"},
                {"name": "agent.thinking"},
                {"name": "agent.message"},
                {"name": "agent.turn_complete"},
                {"name": "agent.child_spawned"},
                {"name": "agent.child_finished"},
                {"name": "tool.called"},
                {"name": "tool.call_delta"},
                {"name": "tool.output"},
                {"name": "tool.result"},
                {"name": "tool.approval_required"},
                {"name": "input.required"},
                {"name": "workflow.started"},
                {"name": "workflow.progress"},
                {"name": "workflow.phase_start"},
                {"name": "workflow.phase_finish"},
                {"name": "workflow.agent_start"},
                {"name": "workflow.agent_finish"},
                {"name": "workflow.completed"},
                {"name": "workflow.failed"},
                {"name": "workflow.cancelled"},
                {"name": "artifact.created"},
                {"name": "payment.required"},
                {"name": "hook.fired"},
                {"name": "budget.warning"},
                {"name": "error"}
            ]
        }
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Protocol types
// ──────────────────────────────────────────────────────────────────────────────

/// An inbound protocol request to be translated to a native command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolRequest {
    pub path: String,
    pub body: Value,
}

/// A native command resulting from protocol translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeCommand {
    pub name: String,
    pub payload: Value,
}

/// A native server event to be translated to a protocol response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeEvent {
    pub event_type: String,
    pub payload: Value,
}

/// A protocol-specific response (translated from a native event).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolResponse {
    pub status: u16,
    pub body: Value,
}

/// Error type for protocol translation failures.
#[derive(Debug, Clone, thiserror::Error)]
#[error("protocol error: {message}")]
pub struct ProtocolError {
    pub message: String,
}

/// Translates between an external protocol and the native Simulacra API.
///
/// Implementations: A2A (agent-as-service), AG-UI (chat UI embedding).
/// S031 defines the trait; implementations are follow-up specs.
#[async_trait]
pub trait ProtocolAdapter: Send + Sync {
    /// Protocol identifier (e.g., "a2a", "ag-ui").
    fn protocol_id(&self) -> &str;

    /// Mount routes on the given axum Router.
    fn routes(&self, engine: Arc<SimulacraEngine>) -> Router;

    /// Translate an inbound protocol message to a native command.
    async fn translate_inbound(
        &self,
        request: ProtocolRequest,
    ) -> Result<NativeCommand, ProtocolError>;

    /// Translate a native event to the protocol's outbound format.
    /// Returns None if the event should not be forwarded.
    async fn translate_outbound(
        &self,
        event: NativeEvent,
    ) -> Result<Option<ProtocolResponse>, ProtocolError>;
}

// ──────────────────────────────────────────────────────────────────────────────
// EventSource trait
// ──────────────────────────────────────────────────────────────────────────────

/// A single message received from an external event source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    pub source_type: String,
    pub source_id: String,
    pub payload: Value,
    pub metadata: HashMap<String, String>,
    pub timestamp: DateTime<Utc>,
}

/// Error type for event source failures.
#[derive(Debug, Clone, thiserror::Error)]
#[error("event source error: {message}")]
pub struct EventSourceError {
    pub message: String,
}

/// Callback type for EventSource — called for each received message.
pub type EventCallback =
    dyn Fn(EventMessage) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync;

/// Pluggable event source for external message systems.
///
/// S032 defines the trait; specific implementations (Kafka, SQS, etc.) are follow-up specs.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Human-readable name of this event source type (e.g., "kafka", "sqs").
    fn source_type(&self) -> &str;

    /// Start consuming events. Calls the provided callback for each event.
    /// Runs until the cancellation token is triggered (graceful shutdown).
    async fn start(
        &self,
        config: Value,
        callback: Box<EventCallback>,
        cancel: CancellationToken,
    ) -> Result<(), EventSourceError>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Backward-compat stub: placeholder_schema
// ──────────────────────────────────────────────────────────────────────────────

/// Returns a stub schema that intentionally returns `ok: false`.
/// Used in red-phase tests only. Use `api_schema()` for the real schema.
#[doc(hidden)]
pub fn placeholder_schema() -> Value {
    serde_json::json!({
        "ok": false,
        "error": {
            "code": "not_implemented",
            "message": "simulacra-server schema not implemented"
        }
    })
}
