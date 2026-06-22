//! HTTP server — axum routes, middleware, WebSocket and REST+SSE transports.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};
use tracing::{error, info, warn};
use uuid::Uuid;

// ── SSE connection tracking ────────────────────────────────────────────────────

/// Stream wrapper that decrements the `active_connections[transport=sse]` gauge
/// when the stream is exhausted or dropped (client disconnect or handler exit).
struct SseTracked {
    inner: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send + 'static>>,
    decremented: bool,
}

impl SseTracked {
    fn new(inner: impl Stream<Item = Result<Event, Infallible>> + Send + 'static) -> Self {
        ServerMeters::get().add_active_connections("sse", 1);
        Self {
            inner: Box::pin(inner),
            decremented: false,
        }
    }
}

impl Stream for SseTracked {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(None) if !self.decremented => {
                self.decremented = true;
                ServerMeters::get().add_active_connections("sse", -1);
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for SseTracked {
    fn drop(&mut self) {
        if !self.decremented {
            self.decremented = true;
            ServerMeters::get().add_active_connections("sse", -1);
        }
    }
}

use crate::auth::{AuthProvider, Credentials, Identity};
use crate::error::ServerError;
use crate::metrics::ServerMeters;
use crate::task::{TaskHandle, TaskManager, TaskManagerError};
use crate::tenant::TenantResolver;
use crate::webhook::{WebhookConfig, WebhookError, WebhookHandler};
use crate::{ProtocolAdapter, api_schema};

use simulacra_catalog::repo::AgentFileRepository;
use simulacra_catalog::{AgentFileStore, ids::AgentFileId, ids::AgentId as CatalogAgentId};
use simulacra_memory::{Embedder, HitIdCache, MemoryStore, VectorIndex};
use simulacra_types::{ArtifactStore, MemoryPath, TenantId};

// ──────────────────────────────────────────────────────────────────────────────
// Config
// ──────────────────────────────────────────────────────────────────────────────

/// Top-level server configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
        }
    }
}

use crate::engine::SimulacraEngine;

// ──────────────────────────────────────────────────────────────────────────────
// Shared app state
// ──────────────────────────────────────────────────────────────────────────────

/// Application state shared across all request handlers.
#[derive(Clone)]
pub struct AppState {
    pub task_manager: Arc<TaskManager>,
    pub resolver: Arc<TenantResolver>,
    pub auth: Arc<dyn AuthProvider>,
    pub engine: Arc<SimulacraEngine>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    /// Global connection-level broadcast (distinct from per-task channels).
    pub event_tx: broadcast::Sender<(String, Value)>,
    /// Webhook configurations — routes are mounted in `build_router`.
    pub webhooks: Vec<WebhookConfig>,
    /// Optional memory storage. When `Some`, the admin ingestion endpoint is
    /// active and memory-enabled agents can use `semantic_search` /
    /// `memory_read_chunk`. When `None`, memory features return 404.
    pub memory_store: Option<Arc<dyn MemoryStore>>,
    pub vector_index: Option<Arc<dyn VectorIndex>>,
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Always present: the tool layer mints hit ids here, and tests may
    /// inspect it. When memory is disabled, the cache exists but is unused.
    pub hit_cache: Arc<HitIdCache>,
    /// S045 — Per-agent files. Both fields move together: the repo manages
    /// metadata + tenant scoping, the store holds bytes. When unset, the
    /// `/api/v1/agents/<id>/files*` endpoints return 503.
    pub agent_file_repo: Option<Arc<dyn AgentFileRepository>>,
    pub agent_file_store: Option<Arc<dyn AgentFileStore>>,
    /// Schedule configurations — surfaced via GET /api/v1/triggers.
    /// The actual Scheduler runs separately as a background task; this
    /// list is the source of truth for what schedules exist.
    pub schedules: Vec<crate::scheduler::ScheduleConfig>,
}

impl AppState {
    pub fn new(
        task_manager: Arc<TaskManager>,
        resolver: Arc<TenantResolver>,
        auth: Arc<dyn AuthProvider>,
    ) -> Self {
        Self::with_webhooks(task_manager, resolver, auth, vec![])
    }

    pub fn with_engine(
        task_manager: Arc<TaskManager>,
        resolver: Arc<TenantResolver>,
        auth: Arc<dyn AuthProvider>,
        engine: Arc<SimulacraEngine>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        // Reuse the engine's artifact store so agent writes and API reads
        // always hit the same backend.
        let artifact_store = Arc::clone(engine.artifact_store());
        // Inherit memory handles from the engine if present. This prevents
        // a misconfig where SimulacraEngine::with_memory was used but
        // AppState::with_engine dropped the handles silently, causing
        // the ingestion endpoint to 404 and agents to get no tools.
        let memory_store = engine.memory_store().cloned();
        let vector_index = engine.vector_index().cloned();
        let embedder = engine.embedder().cloned();
        Self {
            task_manager,
            resolver,
            auth,
            engine,
            artifact_store,
            event_tx,
            webhooks: vec![],
            memory_store,
            vector_index,
            embedder,
            hit_cache: Arc::new(HitIdCache::new()),
            agent_file_repo: None,
            agent_file_store: None,
            schedules: vec![],
        }
    }

    /// Construct an `AppState` with memory handles wired up.
    ///
    /// The memory triple (`memory_store`, `vector_index`, `embedder`) is
    /// required together — a half-wired configuration is rejected at
    /// construction time. The `HitIdCache` is created fresh; production
    /// deployments may share one across multiple `AppState` clones via
    /// `Arc::clone`.
    pub fn with_memory(
        task_manager: Arc<TaskManager>,
        resolver: Arc<TenantResolver>,
        auth: Arc<dyn AuthProvider>,
        engine: Arc<SimulacraEngine>,
        memory_store: Arc<dyn MemoryStore>,
        vector_index: Arc<dyn VectorIndex>,
        embedder: Arc<dyn Embedder>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        let artifact_store = Arc::clone(engine.artifact_store());
        Self {
            task_manager,
            resolver,
            auth,
            engine,
            artifact_store,
            event_tx,
            webhooks: vec![],
            memory_store: Some(memory_store),
            vector_index: Some(vector_index),
            embedder: Some(embedder),
            hit_cache: Arc::new(HitIdCache::new()),
            agent_file_repo: None,
            agent_file_store: None,
            schedules: vec![],
        }
    }

    pub fn with_webhooks(
        task_manager: Arc<TaskManager>,
        resolver: Arc<TenantResolver>,
        auth: Arc<dyn AuthProvider>,
        webhooks: Vec<WebhookConfig>,
    ) -> Self {
        // Construct a minimal engine for backward compat (tests without engine).
        use std::collections::HashMap;
        let config = simulacra_config::SimulacraConfig {
            project: simulacra_config::ProjectConfig {
                name: "simulacra-server".to_string(),
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
        let catalog = simulacra_catalog::Catalog::open_in_memory()
            .expect("in-memory catalog should always open");
        let engine = Arc::new(
            SimulacraEngine::new(
                config,
                None,
                Arc::new(catalog.agents()) as Arc<dyn simulacra_catalog::repo::AgentRepository>,
                Arc::new(catalog.skills()) as Arc<dyn simulacra_catalog::repo::SkillRepository>,
                Arc::new(catalog.memory_pools())
                    as Arc<dyn simulacra_catalog::repo::MemoryPoolRepository>,
                Arc::new(catalog.tenants()) as Arc<dyn simulacra_catalog::repo::TenantRepository>,
            )
            .expect("empty config should always produce a valid engine"),
        );
        let (event_tx, _) = broadcast::channel(1024);
        // Reuse the engine's artifact store.
        let artifact_store = Arc::clone(engine.artifact_store());
        Self {
            task_manager,
            resolver,
            auth,
            engine,
            artifact_store,
            event_tx,
            webhooks,
            memory_store: None,
            vector_index: None,
            embedder: None,
            hit_cache: Arc::new(HitIdCache::new()),
            agent_file_repo: None,
            agent_file_store: None,
            schedules: vec![],
        }
    }

    /// Construct an `AppState` with both webhook and schedule configurations.
    ///
    /// This is the trigger-aware constructor used by the dev server and any
    /// embedder wiring up `GET /api/v1/triggers`. Webhooks are mounted as
    /// routes by `build_router`; schedules are surfaced as data only — the
    /// actual `Scheduler` runs as a separate background task.
    pub fn with_triggers(
        task_manager: Arc<TaskManager>,
        resolver: Arc<TenantResolver>,
        auth: Arc<dyn AuthProvider>,
        webhooks: Vec<WebhookConfig>,
        schedules: Vec<crate::scheduler::ScheduleConfig>,
    ) -> Self {
        let mut state = Self::with_webhooks(task_manager, resolver, auth, webhooks);
        state.schedules = schedules;
        state
    }

    /// S045 — Wire the per-agent files repo + byte store. Keep paired:
    /// the REST handlers need both (repo for metadata + tenant scope, store
    /// for byte payload). Without this call the new endpoints return 503.
    pub fn with_agent_files(
        mut self,
        repo: Arc<dyn AgentFileRepository>,
        store: Arc<dyn AgentFileStore>,
    ) -> Self {
        self.agent_file_repo = Some(repo);
        self.agent_file_store = Some(store);
        self
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// File attachments
// ──────────────────────────────────────────────────────────────────────────────

/// A file to be seeded into the agent workspace before the agent loop starts.
#[derive(Debug, Clone, Deserialize)]
pub struct FileAttachment {
    pub data: String,
    #[serde(default)]
    pub encoding: Option<String>,
}

/// Maximum size for a single decoded attachment: 10 MiB.
const MAX_SINGLE_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
/// Maximum total decoded attachment size: 50 MiB.
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;

/// Validate attachment filenames and sizes, returning decoded bytes on success.
///
/// Rejects:
/// - Empty filenames, filenames starting with `/`, filenames containing `..`
/// - Single files exceeding 10 MiB (decoded)
/// - Total attachment size exceeding 50 MiB (decoded)
/// - Invalid base64 data when encoding is "base64"
pub fn validate_attachments(
    files: &HashMap<String, FileAttachment>,
) -> Result<HashMap<String, Vec<u8>>, (StatusCode, String)> {
    let mut decoded_map = HashMap::with_capacity(files.len());
    let mut total_bytes: usize = 0;

    for (filename, attachment) in files {
        // Validate filename.
        if filename.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "invalid filename: empty string".to_string(),
            ));
        }
        if filename.starts_with('/') {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid filename: absolute path '{filename}'"),
            ));
        }
        if filename.split('/').any(|component| component == "..") {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("invalid filename: parent traversal in '{filename}'"),
            ));
        }

        // Decode content. Only "utf8" (default) and "base64" are valid encodings.
        let bytes = match attachment.encoding.as_deref() {
            None | Some("utf8") => attachment.data.as_bytes().to_vec(),
            Some("base64") => base64::engine::general_purpose::STANDARD
                .decode(&attachment.data)
                .map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("invalid base64 in '{filename}': {e}"),
                    )
                })?,
            Some(other) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "invalid encoding '{other}' for '{filename}': only 'utf8' and 'base64' are supported"
                    ),
                ));
            }
        };

        // Check per-file size.
        if bytes.len() > MAX_SINGLE_ATTACHMENT_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "file '{filename}' is {} bytes, exceeds 10 MiB limit",
                    bytes.len()
                ),
            ));
        }

        total_bytes += bytes.len();
        if total_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "total attachment size exceeds 50 MiB limit".to_string(),
            ));
        }

        decoded_map.insert(filename.clone(), bytes);
    }

    Ok(decoded_map)
}

// ──────────────────────────────────────────────────────────────────────────────
// Route handlers
// ──────────────────────────────────────────────────────────────────────────────

/// GET /health
async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({"ok": true, "status": "healthy"})),
    )
}

/// GET /api/v1/schema
async fn schema() -> impl IntoResponse {
    (StatusCode::OK, Json(api_schema()))
}

/// Request body for task creation.
#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub task: String,
    pub tenant: Option<String>,
    pub agent_type: Option<String>,
    pub metadata: Option<Value>,
    pub files: Option<HashMap<String, FileAttachment>>,
}

/// POST /api/v1/tasks/create
async fn create_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateTaskRequest>,
) -> Response {
    // Auth
    let credentials = extract_credentials(&headers);
    let identity = match state.auth.authenticate(&credentials).await {
        Ok(id) => id,
        Err(e) => {
            warn!(error = %e, "auth failure on task create");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": {"code": "unauthorized", "message": e.to_string()}})),
            )
                .into_response();
        }
    };

    // Tenant resolution
    let tenant = match state.resolver.resolve(&identity) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "tenant resolution failure on task create");
            return (
                StatusCode::FORBIDDEN,
                Json(
                    json!({"ok": false, "error": {"code": "forbidden", "message": e.to_string()}}),
                ),
            )
                .into_response();
        }
    };

    // Validate file attachments (if any) before spawning.
    if let Some(ref files) = body.files
        && !files.is_empty()
        && let Err((status, message)) = validate_attachments(files)
    {
        return (
            status,
            Json(
                json!({"ok": false, "error": {"code": "invalid_attachments", "message": message}}),
            ),
        )
            .into_response();
    }

    let metadata = body.metadata.unwrap_or(json!({}));
    match state
        .engine
        .spawn_task(
            &state.task_manager,
            &body.task,
            tenant,
            body.agent_type.as_deref(),
            metadata,
            body.files,
            None,
        )
        .await
    {
        Ok(handle) => {
            info!(task_id = %handle.task_id, tenant = %handle.tenant, "REST task created");
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "data": {
                        "task_id": handle.task_id,
                        "state": handle.state,
                    }
                })),
            )
                .into_response()
        }
        Err(crate::EngineError::PoolExhausted) => {
            warn!("pool exhausted — returning 503");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok": false, "error": {"code": "pool_exhausted", "message": "worker pool queue is full"}})),
            )
                .into_response()
        }
        Err(crate::EngineError::PoolShutdown) => {
            warn!("pool shutdown — returning 503");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok": false, "error": {"code": "pool_shutdown", "message": "worker pool is shutting down"}})),
            )
                .into_response()
        }
        Err(e) => {
            error!(error = %e, "task creation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": {"code": "internal_error", "message": e.to_string()}})),
            )
                .into_response()
        }
    }
}

/// Query parameters for GET /api/v1/triggers.
#[derive(Debug, Deserialize)]
struct TriggersQuery {
    /// Optional filter — when set, only triggers whose `agent_type` equals
    /// this value are returned.
    agent: Option<String>,
}

/// GET /api/v1/triggers — list webhook + schedule triggers visible to the caller's tenant.
///
/// Filters by tenant namespace (cross-tenant triggers are never returned) and
/// optionally by `agent_type` via the `agent` query param. The response
/// intentionally omits secrets and internal scheduler state — only the env-var
/// presence flag (`hmac`) is exposed for webhooks.
async fn list_triggers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<TriggersQuery>,
) -> Response {
    // Auth
    let credentials = extract_credentials(&headers);
    let identity = match state.auth.authenticate(&credentials).await {
        Ok(id) => id,
        Err(e) => {
            warn!(error = %e, "auth failure on triggers list");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": {"code": "unauthorized", "message": e.to_string()}})),
            )
                .into_response();
        }
    };

    // Tenant resolution
    let tenant = match state.resolver.resolve(&identity) {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "tenant resolution failure on triggers list");
            return (
                StatusCode::FORBIDDEN,
                Json(
                    json!({"ok": false, "error": {"code": "forbidden", "message": e.to_string()}}),
                ),
            )
                .into_response();
        }
    };

    let tenant_ns = tenant.namespace.as_str();
    let agent_filter = params.agent.as_deref();

    let webhooks: Vec<Value> = state
        .webhooks
        .iter()
        .filter(|w| w.tenant == tenant_ns)
        .filter(|w| agent_filter.is_none_or(|a| w.agent_type == a))
        .map(|w| {
            json!({
                "name": w.name,
                "path": w.path,
                "agent_type": w.agent_type,
                "hmac": !w.secret.is_empty(),
            })
        })
        .collect();

    let schedules: Vec<Value> = state
        .schedules
        .iter()
        .filter(|s| s.tenant == tenant_ns)
        .filter(|s| agent_filter.is_none_or(|a| s.agent_type == a))
        .map(|s| {
            let policy = match s.missed_policy {
                crate::scheduler::MissedPolicy::Skip => "skip",
                crate::scheduler::MissedPolicy::RunOnce => "run-once",
                crate::scheduler::MissedPolicy::Backfill => "backfill",
            };
            json!({
                "name": s.name,
                "cron": s.cron,
                "agent_type": s.agent_type,
                "missed_policy": policy,
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "webhooks": webhooks,
            "schedules": schedules,
        })),
    )
        .into_response()
}

/// GET /api/v1/tasks/{task_id}/status — task state
async fn task_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Response {
    let (_, handle) = match resolve_and_check_ownership(&state, &headers, &task_id).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    let path = format!("/api/v1/tasks/{task_id}/status");
    let _span = tracing::info_span!(
        "simulacra_server_request",
        "simulacra.server.method" = "GET",
        "simulacra.server.path" = path.as_str(),
        "simulacra.server.tenant" = handle.tenant.as_str(),
    )
    .entered();

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "data": {
                "task_id": handle.task_id,
                "state": handle.state,
                "tenant": handle.tenant,
                "agent_type": handle.agent_type,
            }
        })),
    )
        .into_response()
}

/// GET /api/v1/tasks/{task_id}/events — SSE stream
async fn task_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Response {
    // Auth + ownership check.
    let (_, _handle) = match resolve_and_check_ownership(&state, &headers, &task_id).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    let (history, rx) = match state.task_manager.subscribe_task(&task_id) {
        Ok(pair) => pair,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    json!({"ok": false, "error": {"code": "not_found", "message": e.to_string()}}),
                ),
            )
                .into_response();
        }
    };

    // Replay history first, then live events. The agent loop typically begins
    // emitting before the browser navigates and opens this SSE stream, so
    // without history a late subscriber would miss tokens, tool calls, and
    // even the terminal state — and see a blank activity feed.
    let history_stream = futures::stream::iter(
        history
            .into_iter()
            .map(Ok::<_, broadcast::error::RecvError>),
    );
    let live_stream =
        BroadcastStream::new(rx).filter_map(|r| futures::future::ready(r.ok().map(Ok)));
    let stream = history_stream
        .chain(live_stream)
        .filter_map(|result: Result<Value, broadcast::error::RecvError>| {
            futures::future::ready(result.ok())
        })
        .scan(false, |closed, payload| {
            if *closed {
                return futures::future::ready(None);
            }
            let is_terminal = payload
                .get("to")
                .and_then(|v| v.as_str())
                .map(|s| matches!(s, "completed" | "failed" | "killed" | "cancelled"))
                .unwrap_or(false);
            if is_terminal {
                *closed = true; // next call returns None, closing the stream
            }
            let data = serde_json::to_string(&payload).unwrap_or_default();
            futures::future::ready(Some(Ok::<Event, Infallible>(Event::default().data(data))))
        });

    Sse::new(SseTracked::new(stream))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// POST /api/v1/tasks/{task_id}/cancel
async fn cancel_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(resp) = resolve_and_check_ownership(&state, &headers, &task_id).await {
        return resp;
    }
    match state.task_manager.cancel_task(&task_id) {
        Ok(s) => (
            StatusCode::OK,
            Json(json!({"ok": true, "data": {"task_id": task_id, "state": s}})),
        )
            .into_response(),
        Err(e) => task_manager_err_response(e),
    }
}

/// POST /api/v1/tasks/{task_id}/pause
async fn pause_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(resp) = resolve_and_check_ownership(&state, &headers, &task_id).await {
        return resp;
    }
    match state.task_manager.pause_task(&task_id) {
        Ok(s) => (
            StatusCode::OK,
            Json(json!({"ok": true, "data": {"task_id": task_id, "state": s}})),
        )
            .into_response(),
        Err(e) => task_manager_err_response(e),
    }
}

/// POST /api/v1/tasks/{task_id}/resume
async fn resume_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(resp) = resolve_and_check_ownership(&state, &headers, &task_id).await {
        return resp;
    }
    match state.task_manager.resume_task(&task_id) {
        Ok(s) => (
            StatusCode::OK,
            Json(json!({"ok": true, "data": {"task_id": task_id, "state": s}})),
        )
            .into_response(),
        Err(e) => task_manager_err_response(e),
    }
}

fn task_manager_err_response(e: TaskManagerError) -> Response {
    let status = match &e {
        TaskManagerError::NotFound { .. } | TaskManagerError::SubscribeFailed { .. } => {
            StatusCode::NOT_FOUND
        }
        TaskManagerError::TerminalState { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(json!({"ok": false, "error": {"code": "task_error", "message": e.to_string()}})),
    )
        .into_response()
}

/// Authenticate the request, resolve tenant, fetch the task, and verify ownership.
///
/// Returns `(Identity, TaskHandle)` on success.
/// Returns an error `Response` (401, 403, or 404) on failure.
async fn resolve_and_check_ownership(
    state: &AppState,
    headers: &HeaderMap,
    task_id: &str,
) -> Result<(Identity, TaskHandle), Response> {
    // 1. Authenticate.
    let creds = extract_credentials(headers);
    let identity = state.auth.authenticate(&creds).await.map_err(|e| {
        warn!(error = %e, "auth failure");
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "error": {"code": "unauthorized", "message": e.to_string()}})),
        )
            .into_response()
    })?;

    // 2. Resolve tenant from identity.
    let tenant = state.resolver.resolve(&identity).map_err(|e| {
        warn!(error = %e, "tenant resolution failure");
        (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": {"code": "forbidden", "message": e.to_string()}})),
        )
            .into_response()
    })?;

    // 3. Fetch the task.
    let handle = state.task_manager.get_task(task_id).map_err(|e| {
        // Return 404 for not-found tasks (don't leak existence to wrong tenants).
        (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": {"code": "not_found", "message": e.to_string()}})),
        )
            .into_response()
    })?;

    // 4. Verify the tenant owns this task.
    if handle.tenant != tenant.namespace {
        warn!(
            task_id = %task_id,
            task_tenant = %handle.tenant,
            request_tenant = %tenant.namespace,
            "tenant ownership mismatch — access denied"
        );
        return Err((
            StatusCode::FORBIDDEN,
            Json(
                json!({"ok": false, "error": {"code": "forbidden", "message": "task does not belong to authenticated tenant"}}),
            ),
        )
            .into_response());
    }

    Ok((identity, handle))
}

// ──────────────────────────────────────────────────────────────────────────────
// Artifact handlers
// ──────────────────────────────────────────────────────────────────────────────

/// Infer a content type from a file extension.
fn infer_content_type(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        Some("html") => "text/html",
        Some("svg") => "image/svg+xml",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

/// GET /api/v1/tasks/{task_id}/artifacts
async fn list_artifacts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Response {
    let (_, handle) = match resolve_and_check_ownership(&state, &headers, &task_id).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    match state.artifact_store.list(handle.tenant.as_str(), &task_id) {
        Ok(entries) => {
            let artifacts: Vec<Value> = entries
                .into_iter()
                .map(|e| {
                    json!({
                        "path": e.path,
                        "size": e.size,
                        "content_type": infer_content_type(&e.path),
                    })
                })
                .collect();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "data": {
                        "artifacts": artifacts,
                    }
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"ok": false, "error": {"code": "internal_error", "message": e.to_string()}}),
            ),
        )
            .into_response(),
    }
}

/// GET /api/v1/tasks/{task_id}/artifacts/*path
async fn get_artifact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((task_id, artifact_path)): Path<(String, String)>,
) -> Response {
    let (_, handle) = match resolve_and_check_ownership(&state, &headers, &task_id).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    match state.artifact_store.get(handle.tenant.as_str(), &task_id, &artifact_path) {
        Ok(data) => {
            let content_type = infer_content_type(&artifact_path);
            let filename = std::path::Path::new(&artifact_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artifact");
            let disposition = format!("inline; filename=\"{filename}\"");

            (
                StatusCode::OK,
                [
                    (
                        axum::http::header::CONTENT_TYPE,
                        content_type.to_string(),
                    ),
                    (axum::http::header::CONTENT_DISPOSITION, disposition),
                ],
                data,
            )
                .into_response()
        }
        Err(simulacra_types::ArtifactError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": {"code": "not_found", "message": format!("artifact not found: {artifact_path}")}})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({"ok": false, "error": {"code": "internal_error", "message": e.to_string()}}),
            ),
        )
            .into_response(),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Memory ingestion (S037 §10)
// ──────────────────────────────────────────────────────────────────────────────
// S045 — per-agent file upload + download
// ──────────────────────────────────────────────────────────────────────────────

/// Per-agent files cap: 50 MiB. Spec S045 §"Upload" — enforced in the
/// upload handler before any blob write reaches the catalog.
const MAX_AGENT_FILE_BYTES: usize = 50 * 1024 * 1024;

fn agent_file_to_json(file: &simulacra_catalog::AgentFile) -> Value {
    json!({
        "id": file.id.0,
        "agentId": file.agent_id.0,
        "name": file.name,
        "mimeType": file.mime_type,
        "sizeBytes": file.size_bytes,
        "downloadUrl": format!(
            "/api/v1/agents/{}/files/{}/bytes",
            file.agent_id.0, file.id.0
        ),
        "createdAt": file.created_at.to_rfc3339(),
        "updatedAt": file.updated_at.to_rfc3339(),
    })
}

/// Resolve the catalog `TenantId` from the request's auth credentials.
///
/// Two-step lookup: auth → tenant namespace (TenantConfig), then catalog
/// `tenants_repo().get_by_namespace(...)` to map namespace → ULID. Errors:
/// 401 on auth failure, 403 on namespace resolution failure, 503 if the
/// catalog tenant is missing (server misconfiguration — the catalog seed
/// is out of sync with the resolver).
async fn resolve_catalog_tenant_id(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<simulacra_catalog::TenantId, Response> {
    let creds = extract_credentials(headers);
    let identity = state.auth.authenticate(&creds).await.map_err(|e| {
        warn!(error = %e, "auth failure on agent_files endpoint");
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "error": {"code": "unauthorized", "message": e.to_string()}})),
        )
            .into_response()
    })?;
    let tenant_cfg = state.resolver.resolve(&identity).map_err(|e| {
        warn!(error = %e, "tenant resolution failure on agent_files endpoint");
        (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": {"code": "forbidden", "message": e.to_string()}})),
        )
            .into_response()
    })?;
    let namespace = tenant_cfg.namespace.clone();
    state
        .engine
        .tenants_repo()
        .get_by_namespace(&namespace)
        .await
        .map(|t| t.id)
        .map_err(|e| {
            warn!(error = %e, namespace = %namespace, "catalog tenant lookup failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": format!("tenant '{namespace}' not in catalog")})),
            )
                .into_response()
        })
}

/// POST /api/v1/agents/:agent_id/files — multipart upload.
async fn upload_agent_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let Some(repo) = state.agent_file_repo.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "agent files not configured"})),
        )
            .into_response();
    };

    let tenant_id = match resolve_catalog_tenant_id(&state, &headers).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    let mut filename: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                if field.name() == Some("file") {
                    filename = field.file_name().map(|s| s.to_string());
                    content_type = field.content_type().map(|s| s.to_string());
                    match field.bytes().await {
                        Ok(b) => bytes = Some(b.to_vec()),
                        Err(e) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(json!({"error": format!("invalid multipart body: {e}")})),
                            )
                                .into_response();
                        }
                    }
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid multipart: {e}")})),
                )
                    .into_response();
            }
        }
    }

    let Some(bytes) = bytes else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing 'file' part"})),
        )
            .into_response();
    };
    let Some(name) = filename else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing filename in 'file' part"})),
        )
            .into_response();
    };
    let mime = content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    if bytes.len() > MAX_AGENT_FILE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": format!("file exceeds maximum size of {MAX_AGENT_FILE_BYTES} bytes"),
            })),
        )
            .into_response();
    }

    let aid = CatalogAgentId(agent_id);
    let result = repo
        .create(
            &tenant_id,
            simulacra_catalog::NewAgentFile {
                agent_id: &aid,
                name: &name,
                mime_type: &mime,
                bytes: &bytes,
            },
        )
        .await;

    match result {
        Ok(file) => (StatusCode::CREATED, Json(agent_file_to_json(&file))).into_response(),
        Err(simulacra_catalog::CatalogError::NotFound(msg)) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": msg}))).into_response()
        }
        Err(simulacra_catalog::CatalogError::Conflict(msg)) => {
            (StatusCode::CONFLICT, Json(json!({"error": msg}))).into_response()
        }
        Err(simulacra_catalog::CatalogError::Validation(msg)) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/v1/agents/:agent_id/files/:file_id/bytes — download.
async fn download_agent_file_bytes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((agent_id, file_id)): Path<(String, String)>,
) -> Response {
    let (Some(repo), Some(store)) = (
        state.agent_file_repo.clone(),
        state.agent_file_store.clone(),
    ) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "agent files not configured"})),
        )
            .into_response();
    };

    let tid = match resolve_catalog_tenant_id(&state, &headers).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let fid = AgentFileId(file_id);

    // Verify tenant scope + that the URL agent_id matches the file's agent_id.
    // Cross-tenant or mismatched agent_id both return 404 — no existence leak.
    let meta = match repo.get(&tid, &fid).await {
        Ok(m) => m,
        Err(simulacra_catalog::CatalogError::NotFound(_)) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    if meta.agent_id.0 != agent_id {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }

    let bytes = match store.get(&fid).await {
        Ok(b) => b,
        Err(simulacra_catalog::CatalogError::NotFound(_)) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, meta.mime_type.clone()),
            (axum::http::header::CONTENT_LENGTH, bytes.len().to_string()),
        ],
        bytes,
    )
        .into_response()
}

// ──────────────────────────────────────────────────────────────────────────────

/// Validate an ingestion source name against `^[a-z0-9][a-z0-9_-]{0,62}$`.
fn is_valid_ingestion_source(s: &str) -> bool {
    if s.is_empty() || s.len() > 63 {
        return false;
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
            return false;
        }
    }
    true
}

#[derive(Debug, Deserialize)]
pub struct IngestionFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct IngestionRequest {
    pub source: String,
    #[serde(default)]
    pub mode: Option<String>,
    pub files: Vec<IngestionFile>,
}

/// Pre-validated shared context for an ingestion request.
///
/// Produced by [`validate_ingest_request`] after successful auth, tenant
/// resolution, and source/mode validation. Both `ingest` and `ingest_stream`
/// consume this so the validation logic lives in one place.
struct IngestContext {
    memory_store: Arc<dyn MemoryStore>,
    tenant: TenantId,
    mode: String,
    source: String,
    source_prefix: String,
    source_prefix_path: MemoryPath,
}

/// Authenticate the request, resolve the tenant, and validate the source/mode
/// fields of the ingestion request body.
///
/// Returns an [`IngestContext`] on success, or a ready-to-return HTTP error
/// response on any validation failure. Both ingestion endpoints call this as
/// their first step so that auth/tenant/source/mode errors surface as normal
/// synchronous HTTP responses (never as SSE error events on a 200 stream).
async fn validate_ingest_request(
    state: &AppState,
    headers: &HeaderMap,
    body: &IngestionRequest,
) -> Result<IngestContext, Response> {
    // Memory enabled?
    let memory_store = state.memory_store.clone().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": {
                    "code": "memory_not_configured",
                    "message": "memory is not configured on this server"
                }
            })),
        )
            .into_response()
    })?;

    // Auth.
    let creds = extract_credentials(headers);
    let identity = state.auth.authenticate(&creds).await.map_err(|e| {
        warn!(error = %e, "auth failure on ingestion");
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "error": {"code": "unauthorized", "message": e.to_string()}})),
        )
            .into_response()
    })?;

    // Tenant resolution.
    let tenant_cfg = state.resolver.resolve(&identity).map_err(|e| {
        warn!(error = %e, "tenant resolution failure on ingestion");
        (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": {"code": "forbidden", "message": e.to_string()}})),
        )
            .into_response()
    })?;

    let tenant = TenantId::parse(&tenant_cfg.namespace).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": {
                    "code": "invalid_tenant",
                    "message": format!("tenant namespace is not a valid TenantId: {e}")
                }
            })),
        )
            .into_response()
    })?;

    // Source.
    if !is_valid_ingestion_source(&body.source) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": {
                    "code": "invalid_source",
                    "message": "source must match ^[a-z0-9][a-z0-9_-]{0,62}$"
                }
            })),
        )
            .into_response());
    }

    // Mode.
    let mode = body.mode.as_deref().unwrap_or("merge").to_string();
    if mode != "merge" && mode != "replace" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": {
                    "code": "invalid_mode",
                    "message": "mode must be 'merge' or 'replace'"
                }
            })),
        )
            .into_response());
    }

    // Prefix path.
    let source_prefix = format!("/mnt/{}", body.source);
    let source_prefix_path = MemoryPath::parse(&source_prefix).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": {
                    "code": "invalid_source",
                    "message": format!("source does not produce a valid MemoryPath: {e}")
                }
            })),
        )
            .into_response()
    })?;

    Ok(IngestContext {
        memory_store,
        tenant,
        mode,
        source: body.source.clone(),
        source_prefix,
        source_prefix_path,
    })
}

/// A file ready to be written: base64-decoded content and a validated
/// [`MemoryPath`] under `/mnt/{source}/`.
struct ValidatedFile {
    full_path: MemoryPath,
    decoded: Vec<u8>,
}

/// Pre-validate (decode base64, check path shape, parse `MemoryPath`) every
/// file in the request before any destructive write runs.
///
/// On any failure, no files are returned — the caller must NOT run
/// `delete_prefix` or any `put` if this returns `Err`. The error carries the
/// code/message that should surface to the client (via HTTP response for the
/// synchronous handler or an `ingestion.error` SSE event for the streaming
/// handler).
fn prevalidate_files(
    source: &str,
    files: &[IngestionFile],
) -> Result<Vec<ValidatedFile>, (String, String)> {
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&file.content)
            .map_err(|e| {
                (
                    "invalid_base64".to_string(),
                    format!("file '{}': {e}", file.path),
                )
            })?;

        if file.path.is_empty() || file.path.starts_with('/') || file.path.contains("..") {
            return Err((
                "invalid_file_path".to_string(),
                format!("invalid file path: '{}'", file.path),
            ));
        }

        let full_path_str = format!("/mnt/{}/{}", source, file.path);
        let full_path = MemoryPath::parse(&full_path_str).map_err(|e| {
            (
                "invalid_file_path".to_string(),
                format!("'{}': {e}", file.path),
            )
        })?;

        out.push(ValidatedFile { full_path, decoded });
    }
    Ok(out)
}

/// POST /api/v1/ingestion — admin memory ingestion.
///
/// Writes files into `/mnt/{source}/{file.path}` for the authenticated
/// tenant. Mode `"merge"` (default) upserts per file; mode `"replace"` calls
/// `delete_prefix` on `/mnt/{source}/` before writing the new set.
///
/// Returns 404 if memory is not configured on the server.
async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IngestionRequest>,
) -> Response {
    let ctx = match validate_ingest_request(&state, &headers, &body).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    // Pre-validate every file before doing anything destructive. This keeps
    // the behavior symmetric with `/api/v1/ingestion/stream` and prevents a
    // mid-request failure from leaving the prefix cleared but the new files
    // unwritten.
    let validated = match prevalidate_files(&ctx.source, &body.files) {
        Ok(v) => v,
        Err((code, message)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": { "code": code, "message": message }
                })),
            )
                .into_response();
        }
    };

    // Replace mode: clear the prefix first. Blocking SQLite work runs on the
    // blocking pool, not a tokio worker thread.
    if ctx.mode == "replace" {
        let store = Arc::clone(&ctx.memory_store);
        let tenant = ctx.tenant.clone();
        let prefix_path = ctx.source_prefix_path.clone();
        let result =
            tokio::task::spawn_blocking(move || store.delete_prefix(&tenant, &prefix_path))
                .await
                .expect("delete_prefix task panicked");
        if let Err(e) = result {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "ok": false,
                    "error": {
                        "code": "delete_prefix_failed",
                        "message": e.to_string()
                    }
                })),
            )
                .into_response();
        }
    }

    // Write files.
    let mut written = Vec::with_capacity(validated.len());
    for file in validated {
        let store = Arc::clone(&ctx.memory_store);
        let tenant = ctx.tenant.clone();
        let path = file.full_path.clone();
        let data = file.decoded;
        let result = tokio::task::spawn_blocking(move || store.put(&tenant, &path, &data))
            .await
            .expect("put task panicked");
        match result {
            Ok(version) => {
                written.push(json!({
                    "path": file.full_path.as_str(),
                    "version": version.0,
                }));
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "ok": false,
                        "error": {
                            "code": "put_failed",
                            "message": e.to_string()
                        }
                    })),
                )
                    .into_response();
            }
        }
    }

    info!(
        tenant = %ctx.tenant,
        source = %ctx.source,
        mode = %ctx.mode,
        count = written.len(),
        "ingestion complete"
    );

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "data": {
                "source": ctx.source,
                "mode": ctx.mode,
                "written": written,
            }
        })),
    )
        .into_response()
}

/// Best-effort event emission to the SSE observer.
///
/// Once the observer is detected closed (send error), `observer_open` flips
/// to `false` and subsequent calls skip the send. This decouples the observer
/// lifetime from the underlying ingest worker: a client disconnect mid-stream
/// never aborts an in-flight ingest.
async fn emit_ingest_event(
    tx: &tokio::sync::mpsc::Sender<Value>,
    observer_open: &mut bool,
    event: Value,
) {
    if *observer_open && tx.send(event).await.is_err() {
        *observer_open = false;
    }
}

/// POST /api/v1/ingestion/stream — SSE variant of `/api/v1/ingestion`.
///
/// Emits a task-like event stream aligned with the `task.state_changed`
/// envelope used by `/api/v1/tasks/:task_id/events`. Each payload carries
/// `event`, `ingestion_id`, and a monotonic `seq`:
///
/// ```text
/// { "event": "ingestion.started",   "ingestion_id": "<uuid>", "seq": 1, "source": "...", "mode": "...", "file_count": N }
/// { "event": "ingestion.cleared",   "ingestion_id": "<uuid>", "seq": 2, "prefix": "/mnt/..." }   // only on mode=replace
/// { "event": "ingestion.written",   "ingestion_id": "<uuid>", "seq": N, "path": "...", "version": N }
/// { "event": "ingestion.completed", "ingestion_id": "<uuid>", "seq": N, "count": N }
/// ```
///
/// Terminal failures during the stream surface as:
///
/// ```text
/// { "event": "ingestion.error", "ingestion_id": "<uuid>", "seq": N, "code": "...", "message": "..." }
/// ```
///
/// Auth / tenant / source / mode validation errors return a normal HTTP
/// error response (SSE stream not opened).
///
/// Per-file validation (base64 decode, path shape, `MemoryPath::parse`) runs
/// as a synchronous pre-flight pass BEFORE the SSE stream is opened. If any
/// file fails pre-validation, the handler returns a synchronous HTTP 400
/// with an `invalid_base64` or `invalid_file_path` error code (content-type
/// `application/json`, NOT `text/event-stream`) and does not spawn the
/// worker — `delete_prefix` and `put` never run.
///
/// Only post-validation errors surface as `ingestion.error` events inside
/// the stream and terminate it early: specifically `delete_prefix_failed`
/// and `put_failed` from the store layer.
///
/// A client disconnect mid-stream does NOT abort the ingest. The worker task
/// continues running to completion; the observer channel is treated as a
/// best-effort transport, and a terminal log line records what landed.
async fn ingest_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IngestionRequest>,
) -> Response {
    let ctx = match validate_ingest_request(&state, &headers, &body).await {
        Ok(c) => c,
        Err(resp) => return resp,
    };

    // Pre-validate every file before opening the stream. A client that
    // supplied bad base64 or a malformed path gets a synchronous 400 — no
    // destructive work runs and no SSE stream is opened.
    let validated = match prevalidate_files(&ctx.source, &body.files) {
        Ok(v) => v,
        Err((code, message)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": { "code": code, "message": message }
                })),
            )
                .into_response();
        }
    };

    // Channel capacity bounded at 64 — a slow SSE consumer back-pressures
    // the producer rather than dropping events or blowing memory.
    let (tx, rx) = tokio::sync::mpsc::channel::<Value>(64);
    let file_count = validated.len();
    let ingestion_id = Uuid::new_v4().to_string();

    let IngestContext {
        memory_store,
        tenant,
        mode,
        source,
        source_prefix,
        source_prefix_path,
    } = ctx;

    let ingestion_id_worker = ingestion_id.clone();

    tokio::spawn(async move {
        let mut observer_open = true;
        let mut observer_disconnect_logged = false;
        let mut seq: u64 = 0;
        let mut next_seq = || {
            seq += 1;
            seq
        };

        // Helper: runs after every emit_ingest_event call. Fires the
        // disconnect log exactly once on the first observed transition to
        // closed so operators see the worker is still ingesting without an
        // audience, even if every subsequent put fails.
        let maybe_log_disconnect = |observer_open: bool,
                                    disconnect_logged: &mut bool,
                                    tenant: &TenantId,
                                    source: &str,
                                    mode: &str,
                                    ingestion_id: &str| {
            if !observer_open && !*disconnect_logged {
                *disconnect_logged = true;
                info!(
                    tenant = %tenant,
                    source = %source,
                    mode = %mode,
                    ingestion_id = %ingestion_id,
                    "ingestion stream observer disconnected, continuing ingest"
                );
            }
        };

        // `started`
        emit_ingest_event(
            &tx,
            &mut observer_open,
            json!({
                "event": "ingestion.started",
                "ingestion_id": ingestion_id_worker,
                "seq": next_seq(),
                "source": source,
                "mode": mode,
                "file_count": file_count,
            }),
        )
        .await;
        maybe_log_disconnect(
            observer_open,
            &mut observer_disconnect_logged,
            &tenant,
            &source,
            &mode,
            &ingestion_id_worker,
        );

        // Replace mode: clear the prefix before writing.
        if mode == "replace" {
            let store = Arc::clone(&memory_store);
            let tenant_for_delete = tenant.clone();
            let prefix_path = source_prefix_path.clone();
            let delete_result = tokio::task::spawn_blocking(move || {
                store.delete_prefix(&tenant_for_delete, &prefix_path)
            })
            .await
            .unwrap_or_else(|join_err| {
                Err(simulacra_memory::MemoryError::Internal(format!(
                    "delete_prefix task panicked: {join_err}"
                )))
            });

            if let Err(e) = delete_result {
                emit_ingest_event(
                    &tx,
                    &mut observer_open,
                    json!({
                        "event": "ingestion.error",
                        "ingestion_id": ingestion_id_worker,
                        "seq": next_seq(),
                        "code": "delete_prefix_failed",
                        "message": e.to_string(),
                    }),
                )
                .await;
                maybe_log_disconnect(
                    observer_open,
                    &mut observer_disconnect_logged,
                    &tenant,
                    &source,
                    &mode,
                    &ingestion_id_worker,
                );
                warn!(
                    tenant = %tenant,
                    source = %source,
                    mode = %mode,
                    code = "delete_prefix_failed",
                    message = %e,
                    ingestion_id = %ingestion_id_worker,
                    "ingestion stream failed"
                );
                return;
            }

            emit_ingest_event(
                &tx,
                &mut observer_open,
                json!({
                    "event": "ingestion.cleared",
                    "ingestion_id": ingestion_id_worker,
                    "seq": next_seq(),
                    "prefix": source_prefix,
                }),
            )
            .await;
            maybe_log_disconnect(
                observer_open,
                &mut observer_disconnect_logged,
                &tenant,
                &source,
                &mode,
                &ingestion_id_worker,
            );
        }

        // Write files. Blocking SQLite work runs on the blocking pool so it
        // cannot starve async tasks on the tokio worker threads.
        let mut written = 0usize;

        for file in validated {
            let store = Arc::clone(&memory_store);
            let tenant_for_put = tenant.clone();
            let path = file.full_path.clone();
            let data = file.decoded;
            let put_result =
                tokio::task::spawn_blocking(move || store.put(&tenant_for_put, &path, &data))
                    .await
                    .unwrap_or_else(|join_err| {
                        Err(simulacra_memory::MemoryError::Internal(format!(
                            "put task panicked: {join_err}"
                        )))
                    });

            match put_result {
                Ok(version) => {
                    emit_ingest_event(
                        &tx,
                        &mut observer_open,
                        json!({
                            "event": "ingestion.written",
                            "ingestion_id": ingestion_id_worker,
                            "seq": next_seq(),
                            "path": file.full_path.as_str(),
                            "version": version.0,
                        }),
                    )
                    .await;
                    maybe_log_disconnect(
                        observer_open,
                        &mut observer_disconnect_logged,
                        &tenant,
                        &source,
                        &mode,
                        &ingestion_id_worker,
                    );
                    written += 1;
                }
                Err(e) => {
                    emit_ingest_event(
                        &tx,
                        &mut observer_open,
                        json!({
                            "event": "ingestion.error",
                            "ingestion_id": ingestion_id_worker,
                            "seq": next_seq(),
                            "code": "put_failed",
                            "message": e.to_string(),
                        }),
                    )
                    .await;
                    maybe_log_disconnect(
                        observer_open,
                        &mut observer_disconnect_logged,
                        &tenant,
                        &source,
                        &mode,
                        &ingestion_id_worker,
                    );
                    warn!(
                        tenant = %tenant,
                        source = %source,
                        mode = %mode,
                        code = "put_failed",
                        message = %e,
                        ingestion_id = %ingestion_id_worker,
                        "ingestion stream failed"
                    );
                    return;
                }
            }
        }

        emit_ingest_event(
            &tx,
            &mut observer_open,
            json!({
                "event": "ingestion.completed",
                "ingestion_id": ingestion_id_worker,
                "seq": next_seq(),
                "count": written,
            }),
        )
        .await;
        maybe_log_disconnect(
            observer_open,
            &mut observer_disconnect_logged,
            &tenant,
            &source,
            &mode,
            &ingestion_id_worker,
        );

        info!(
            tenant = %tenant,
            source = %source,
            mode = %mode,
            count = written,
            ingestion_id = %ingestion_id_worker,
            "ingestion stream complete"
        );
    });

    let stream = ReceiverStream::new(rx).map(|event| {
        let data = serde_json::to_string(&event).unwrap_or_default();
        Ok::<Event, Infallible>(Event::default().data(data))
    });

    Sse::new(SseTracked::new(stream))
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ──────────────────────────────────────────────────────────────────────────────
// WebSocket handler
// ──────────────────────────────────────────────────────────────────────────────

/// GET /api/v1/ws — WebSocket upgrade
async fn websocket_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let creds = extract_credentials(&headers);
    match state.auth.authenticate(&creds).await {
        Ok(identity) => {
            let conn_id = Uuid::new_v4().to_string();
            ws.on_upgrade(move |socket| handle_websocket(socket, state, identity, conn_id))
        }
        Err(e) => {
            warn!(error = %e, "WebSocket auth failure");
            StatusCode::UNAUTHORIZED.into_response()
        }
    }
}

async fn handle_websocket(
    mut socket: WebSocket,
    state: AppState,
    identity: Identity,
    conn_id: String,
) {
    ServerMeters::get().add_active_connections("ws", 1);

    while let Some(msg) = socket.recv().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };

        match msg {
            Message::Text(text) => {
                let response = process_ws_command(&text, &state, &identity, &conn_id).await;
                let json = serde_json::to_string(&response).unwrap_or_default();
                if socket.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => {
                let cancelled = state.task_manager.cancel_connection_tasks(&conn_id);
                info!(
                    conn_id = %conn_id,
                    cancelled_count = cancelled.len(),
                    "WebSocket closed — cancelled connection tasks"
                );
                break;
            }
            _ => {}
        }
    }
    // Cancel any remaining tasks for this connection on disconnect.
    let remaining = state.task_manager.cancel_connection_tasks(&conn_id);
    if !remaining.is_empty() {
        info!(
            conn_id = %conn_id,
            cancelled_count = remaining.len(),
            "WebSocket disconnected — cancelled remaining connection tasks"
        );
    }

    ServerMeters::get().add_active_connections("ws", -1);
}

/// Verify that `identity` owns the task identified by `task_id`.
///
/// Returns the `TaskHandle` on success, or an error-event `Value` on failure.
fn check_ws_ownership(
    task_manager: &TaskManager,
    resolver: &TenantResolver,
    identity: &Identity,
    task_id: &str,
) -> Result<TaskHandle, Value> {
    let handle = task_manager.get_task(task_id).map_err(|_| {
        json!({"event": "error", "code": "not_found", "message": format!("task '{task_id}' not found")})
    })?;
    let tenant = resolver.resolve(identity).map_err(
        |_| json!({"event": "error", "code": "forbidden", "message": "tenant resolution failed"}),
    )?;
    if handle.tenant != tenant.namespace {
        return Err(json!({"event": "error", "code": "forbidden", "message": "access denied"}));
    }
    Ok(handle)
}

async fn process_ws_command(
    text: &str,
    state: &AppState,
    identity: &Identity,
    conn_id: &str,
) -> Value {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct WsCommand {
        command: String,
        task_id: Option<String>,
        task: Option<String>,
        agent_type: Option<String>,
        metadata: Option<Value>,
        content: Option<String>,
        tool_call_id: Option<String>,
        approved: Option<bool>,
        reason: Option<String>,
    }

    let cmd: WsCommand = match serde_json::from_str(text) {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "event": "error",
                "code": "invalid_message",
                "message": format!("malformed command: {e}")
            });
        }
    };

    match cmd.command.as_str() {
        "task.create" => {
            let task_desc = cmd.task.unwrap_or_default();
            let tenant = match state.resolver.resolve(identity) {
                Ok(t) => t,
                Err(e) => {
                    return json!({
                        "event": "error",
                        "code": "forbidden",
                        "message": e.to_string()
                    });
                }
            };
            match state
                .engine
                .spawn_task(
                    &state.task_manager,
                    &task_desc,
                    tenant,
                    cmd.agent_type.as_deref(),
                    cmd.metadata.unwrap_or_default(),
                    None,
                    Some(conn_id.to_string()),
                )
                .await
            {
                Ok(handle) => json!({
                    "event": "task.created",
                    "task_id": handle.task_id,
                    "state": handle.state,
                }),
                Err(e) => json!({
                    "event": "error",
                    "code": "task_error",
                    "message": e.to_string()
                }),
            }
        }
        "task.cancel" => {
            let task_id = cmd.task_id.unwrap_or_default();
            if let Err(e) =
                check_ws_ownership(&state.task_manager, &state.resolver, identity, &task_id)
            {
                return e;
            }
            match state.task_manager.cancel_task(&task_id) {
                Ok(s) => json!({
                    "event": "task.state_changed",
                    "task_id": task_id,
                    "to": s.to_string()
                }),
                Err(e) => json!({
                    "event": "error",
                    "task_id": task_id,
                    "code": "task_error",
                    "message": e.to_string()
                }),
            }
        }
        "task.pause" => {
            let task_id = cmd.task_id.unwrap_or_default();
            if let Err(e) =
                check_ws_ownership(&state.task_manager, &state.resolver, identity, &task_id)
            {
                return e;
            }
            match state.task_manager.pause_task(&task_id) {
                Ok(s) => json!({
                    "event": "task.state_changed",
                    "task_id": task_id,
                    "to": s.to_string()
                }),
                Err(e) => json!({
                    "event": "error",
                    "task_id": task_id,
                    "code": "task_error",
                    "message": e.to_string()
                }),
            }
        }
        "task.resume" => {
            let task_id = cmd.task_id.unwrap_or_default();
            if let Err(e) =
                check_ws_ownership(&state.task_manager, &state.resolver, identity, &task_id)
            {
                return e;
            }
            match state.task_manager.resume_task(&task_id) {
                Ok(s) => json!({
                    "event": "task.state_changed",
                    "task_id": task_id,
                    "to": s.to_string()
                }),
                Err(e) => json!({
                    "event": "error",
                    "task_id": task_id,
                    "code": "task_error",
                    "message": e.to_string()
                }),
            }
        }
        "input.response" => {
            let task_id = cmd.task_id.unwrap_or_default();
            if let Err(e) =
                check_ws_ownership(&state.task_manager, &state.resolver, identity, &task_id)
            {
                return e;
            }
            let content = cmd.content.unwrap_or_default();
            match state.task_manager.provide_input(&task_id, &content) {
                Ok(s) => json!({
                    "event": "task.state_changed",
                    "task_id": task_id,
                    "to": s.to_string()
                }),
                Err(e) => json!({
                    "event": "error",
                    "task_id": task_id,
                    "code": "task_error",
                    "message": e.to_string()
                }),
            }
        }
        "approval.respond" => {
            let task_id = cmd.task_id.unwrap_or_default();
            if let Err(e) =
                check_ws_ownership(&state.task_manager, &state.resolver, identity, &task_id)
            {
                return e;
            }
            let tool_call_id = cmd.tool_call_id.unwrap_or_default();
            let approved = cmd.approved.unwrap_or(false);
            match state.task_manager.respond_approval(
                &task_id,
                &tool_call_id,
                approved,
                cmd.reason.as_deref(),
            ) {
                Ok(s) => json!({
                    "event": "task.state_changed",
                    "task_id": task_id,
                    "to": s.to_string()
                }),
                Err(TaskManagerError::ApprovalDenied { .. }) => json!({
                    "event": "tool.result",
                    "task_id": task_id,
                    "result": "denied",
                    "code": "approval_denied"
                }),
                Err(e) => json!({
                    "event": "error",
                    "task_id": task_id,
                    "code": "task_error",
                    "message": e.to_string()
                }),
            }
        }
        unknown => json!({
            "event": "error",
            "code": "invalid_message",
            "message": format!("unknown command: {unknown}")
        }),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Router builder
// ──────────────────────────────────────────────────────────────────────────────

/// Optional GraphQL mount for [`build_router`].
///
/// `build_router` does NOT assemble the schema itself — schema construction
/// (data injection, repository wiring) is the caller's responsibility. A caller
/// that wants the GraphQL control plane mounted at `/graphql` constructs a
/// [`simulacra_graphql::schema::SimulacraSchema`], picks an
/// [`simulacra_graphql::auth::GraphQLAuthProvider`], builds a
/// [`simulacra_graphql::context::TenantResolver`], and passes them in via this
/// struct. The dev_server example demonstrates the full assembly.
pub struct GraphQLMount {
    pub schema: simulacra_graphql::schema::SimulacraSchema,
    pub auth: std::sync::Arc<dyn simulacra_graphql::auth::GraphQLAuthProvider>,
    pub tenant_resolver: simulacra_graphql::context::TenantResolver,
}

/// Build the main application router.
pub fn build_router(
    state: AppState,
    adapters: Vec<Box<dyn ProtocolAdapter>>,
    graphql: Option<GraphQLMount>,
) -> Router {
    // 75 MiB body limit to accommodate 50 MiB decoded attachments
    // (base64 inflation + JSON envelope overhead).
    let body_limit = axum::extract::DefaultBodyLimit::max(75 * 1024 * 1024);
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/api/v1/schema", get(schema))
        .route("/api/v1/ws", get(websocket_upgrade))
        .route("/api/v1/tasks/create", post(create_task))
        .route("/api/v1/triggers", get(list_triggers))
        .route("/api/v1/tasks/:task_id/status", get(task_status))
        .route("/api/v1/tasks/:task_id/events", get(task_events))
        .route("/api/v1/tasks/:task_id/cancel", post(cancel_task))
        .route("/api/v1/tasks/:task_id/pause", post(pause_task))
        .route("/api/v1/tasks/:task_id/resume", post(resume_task))
        .route("/api/v1/tasks/:task_id/artifacts", get(list_artifacts))
        .route("/api/v1/tasks/:task_id/artifacts/*path", get(get_artifact))
        .route("/api/v1/ingestion", post(ingest))
        .route("/api/v1/ingestion/stream", post(ingest_stream))
        .route("/api/v1/agents/:agent_id/files", post(upload_agent_file))
        .route(
            "/api/v1/agents/:agent_id/files/:file_id/bytes",
            get(download_agent_file_bytes),
        )
        .layer(body_limit)
        .with_state(state.clone());

    // Mount webhook routes — one per webhook config.
    for webhook_config in &state.webhooks {
        let path = webhook_config.path.clone();
        let wh_state = state.clone();
        let wh_config = webhook_config.clone();
        router = router.route(
            &path,
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let wh_state = wh_state.clone();
                let wh_config = wh_config.clone();
                async move {
                    let handler = WebhookHandler::new(wh_config);
                    let sig = headers
                        .get("x-simulacra-signature")
                        .and_then(|v| v.to_str().ok());
                    // Route through the engine so a real agent is spawned —
                    // bare `TaskManager::create_task` would produce a record
                    // without a running worker.
                    match handler
                        .process_with_engine(
                            &body,
                            sig,
                            &wh_state.engine,
                            &wh_state.task_manager,
                            &wh_state.resolver,
                        )
                        .await
                    {
                        Ok(handle) => (StatusCode::OK, Json(json!({"task_id": handle.task_id})))
                            .into_response(),
                        Err(WebhookError::MissingSignature)
                        | Err(WebhookError::InvalidSignature) => {
                            StatusCode::UNAUTHORIZED.into_response()
                        }
                        Err(WebhookError::InvalidBody(_)) => {
                            StatusCode::BAD_REQUEST.into_response()
                        }
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": e.to_string()})),
                        )
                            .into_response(),
                    }
                }
            }),
        );
    }

    // Mount protocol adapter routes (adapter routes don't get state — they build their own).
    for adapter in &adapters {
        let adapter_router = adapter.routes(Arc::clone(&state.engine));
        router = router.merge(adapter_router);
    }

    // Mount GraphQL BEFORE the frontend so the `/graphql` POST route is not
    // shadowed by the frontend's `/` SPA fallback. The frontend MUST be the
    // last merge so its catch-all does not eclipse anything.
    if let Some(mount) = graphql {
        router = router.merge(simulacra_graphql::graphql_router(
            mount.schema,
            mount.auth,
            mount.tenant_resolver,
        ));
    }
    router = router.merge(simulacra_frontend::frontend_router());

    router
}

// ──────────────────────────────────────────────────────────────────────────────
// Server entry point
// ──────────────────────────────────────────────────────────────────────────────

/// The Simulacra API server.
pub struct SimulacraServer {
    config: ServerConfig,
    state: AppState,
    adapters: Vec<Box<dyn ProtocolAdapter>>,
}

impl SimulacraServer {
    pub fn new(
        config: ServerConfig,
        state: AppState,
        adapters: Vec<Box<dyn ProtocolAdapter>>,
    ) -> Self {
        Self {
            config,
            state,
            adapters,
        }
    }

    /// Start listening and serving requests.
    pub async fn run(self) -> Result<(), ServerError> {
        let addr: SocketAddr = format!("{}:{}", self.config.host, self.config.port)
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                ServerError::Config(format!("invalid bind address: {e}"))
            })?;

        let tenant_count = self.state.resolver.tenant_count();
        let adapter_count = self.adapters.len();
        let router = build_router(self.state, self.adapters, None);

        info!(
            bind_address = %addr,
            tenant_count = tenant_count,
            adapter_count = adapter_count,
            "simulacra-server starting"
        );

        let listener =
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| ServerError::Bind {
                    addr: addr.to_string(),
                    source: e,
                })?;

        axum::serve(listener, router).await.map_err(ServerError::Io)
    }
}

/// Convenience function for `simulacra serve`.
pub async fn start_server(
    config: ServerConfig,
    auth: Arc<dyn AuthProvider>,
    resolver: TenantResolver,
) -> Result<(), ServerError> {
    let task_manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(resolver);
    let state = AppState::new(task_manager, resolver, auth);
    let server = SimulacraServer::new(config, state, vec![]);
    server.run().await
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Extract credentials from the Authorization header.
fn extract_credentials(headers: &HeaderMap) -> Credentials {
    if let Some(auth) = headers.get("authorization")
        && let Ok(value) = auth.to_str()
    {
        if let Some(token) = value.strip_prefix("Bearer ") {
            return Credentials::Bearer(token.to_string());
        }
        if let Some(key) = value.strip_prefix("ApiKey ") {
            return Credentials::ApiKey(key.to_string());
        }
    }
    // No credentials — return empty bearer to trigger MissingCredentials.
    Credentials::Bearer(String::new())
}
