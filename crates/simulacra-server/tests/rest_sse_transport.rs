//! Tests for REST + SSE transport (S031 assertions).

use axum_test::TestServer;
use simulacra_server::{BudgetPoolConfig, api_schema};

// ─── Schema assertions ────────────────────────────────────────────────────────

#[test]
fn api_schema_returns_ok_true_with_data_field() {
    let schema = api_schema();
    assert_eq!(
        schema["ok"],
        serde_json::json!(true),
        "api_schema must return ok: true"
    );
    assert!(
        schema.get("data").is_some(),
        "api_schema must include data field with command/event schema"
    );
}

#[test]
fn api_schema_data_contains_commands_and_events_arrays() {
    let schema = api_schema();
    let data = &schema["data"];
    assert!(
        data.get("commands").is_some(),
        "schema.data must contain commands array"
    );
    assert!(
        data.get("events").is_some(),
        "schema.data must contain events array"
    );

    let commands = data["commands"].as_array().unwrap();
    let events = data["events"].as_array().unwrap();

    assert!(!commands.is_empty(), "commands array must not be empty");
    assert!(!events.is_empty(), "events array must not be empty");
}

#[test]
fn api_schema_includes_all_required_commands() {
    let schema = api_schema();
    let commands = schema["data"]["commands"].as_array().unwrap();
    let command_names: Vec<&str> = commands.iter().filter_map(|c| c["name"].as_str()).collect();

    let required = [
        "task.create",
        "task.cancel",
        "task.pause",
        "task.resume",
        "input.response",
        "approval.respond",
    ];
    for cmd in &required {
        assert!(
            command_names.contains(cmd),
            "api_schema must include command: {cmd}"
        );
    }
}

#[test]
fn api_schema_includes_all_required_events() {
    let schema = api_schema();
    let events = schema["data"]["events"].as_array().unwrap();
    let event_names: Vec<&str> = events.iter().filter_map(|e| e["name"].as_str()).collect();

    let required = [
        "task.state_changed",
        "agent.thinking",
        "agent.message",
        "tool.called",
        "tool.result",
        "tool.approval_required",
        "input.required",
        "artifact.created",
        "payment.required",
        "hook.fired",
        "budget.warning",
        "error",
    ];
    for ev in &required {
        assert!(
            event_names.contains(ev),
            "api_schema must include event: {ev}"
        );
    }
}

// ─── Router structural assertions ─────────────────────────────────────────────

#[test]
fn build_router_produces_a_valid_axum_router() {
    use simulacra_server::{
        AppState, TaskManager, TenantResolver, auth::CompositeAuthProvider, build_router,
    };
    use std::sync::Arc;

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::default());
    let auth: Arc<dyn simulacra_server::AuthProvider> =
        Arc::new(CompositeAuthProvider::new(vec![]));
    let state = AppState::new(manager, resolver, auth);

    // build_router must not panic with no adapters.
    let _router = build_router(state, vec![], None);
}

// ─── ApiResponse envelope assertions ──────────────────────────────────────────

#[test]
fn api_response_ok_wraps_data_with_ok_true() {
    use simulacra_server::ApiResponse;
    let response = ApiResponse::ok(serde_json::json!({"task_id": "task-1"}));
    assert!(response.ok, "ok response must have ok: true");
    assert!(response.data.is_some(), "ok response must have data");
    assert!(response.error.is_none(), "ok response must not have error");
}

#[test]
fn api_response_err_wraps_error_with_ok_false() {
    use simulacra_server::ApiResponse;
    let response = ApiResponse::err("not_found", "task not found");
    assert!(!response.ok, "error response must have ok: false");
    assert!(response.data.is_none(), "error response must not have data");
    let error = response.error.unwrap();
    assert_eq!(error.code, "not_found");
    assert_eq!(error.message, "task not found");
}

// ─── Health endpoint assertion ────────────────────────────────────────────────

#[tokio::test]
async fn health_endpoint_returns_200_ok() {
    use simulacra_server::{
        AppState, TaskManager, TenantResolver, auth::CompositeAuthProvider, build_router,
    };
    use std::sync::Arc;

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::default());
    let auth: Arc<dyn simulacra_server::AuthProvider> =
        Arc::new(CompositeAuthProvider::new(vec![]));
    let state = AppState::new(manager, resolver, auth);
    let router = build_router(state, vec![], None);

    let server = TestServer::new(router).unwrap();
    let response = server.get("/health").await;
    response.assert_status_ok();
}

// ─── Schema endpoint assertion ────────────────────────────────────────────────

#[tokio::test]
async fn schema_endpoint_returns_200_with_command_and_event_schema() {
    use simulacra_server::{
        AppState, TaskManager, TenantResolver, auth::CompositeAuthProvider, build_router,
    };
    use std::sync::Arc;

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::default());
    let auth: Arc<dyn simulacra_server::AuthProvider> =
        Arc::new(CompositeAuthProvider::new(vec![]));
    let state = AppState::new(manager, resolver, auth);
    let router = build_router(state, vec![], None);

    let server = TestServer::new(router).unwrap();
    let response = server.get("/api/v1/schema").await;
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    assert_eq!(body["ok"], serde_json::json!(true));
    assert!(body["data"].get("commands").is_some());
    assert!(body["data"].get("events").is_some());
}

// ─── BLOCKER 1: Tenant ownership enforcement ───────────────────────────────────

#[tokio::test]
async fn cancel_task_returns_403_when_task_belongs_to_different_tenant() {
    use simulacra_server::{
        ApiKeyAuthProvider, ApiKeyEntry, AppState, TaskManager, TaskState, TenantConfig,
        TenantResolver, build_router,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    // Two tenants.
    let mut tenants = HashMap::new();
    tenants.insert(
        "tenant-a".to_string(),
        TenantConfig {
            namespace: "tenant-a".to_string(),
            agent_type: "agent-a".to_string(),
            vfs_root: PathBuf::from("/data/a"),
            budget_pool: BudgetPoolConfig::default(),
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );
    tenants.insert(
        "tenant-b".to_string(),
        TenantConfig {
            namespace: "tenant-b".to_string(),
            agent_type: "agent-b".to_string(),
            vfs_root: PathBuf::from("/data/b"),
            budget_pool: BudgetPoolConfig::default(),
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants.clone(), None));

    // Two API keys — one per tenant.
    let auth: Arc<dyn simulacra_server::AuthProvider> =
        Arc::new(ApiKeyAuthProvider::from_entries(vec![
            ApiKeyEntry {
                key: "key-a".to_string(),
                subject: "user-a".to_string(),
                tenant_namespace: Some("tenant-a".to_string()),
                scopes: vec!["tasks:manage".to_string()],
            },
            ApiKeyEntry {
                key: "key-b".to_string(),
                subject: "user-b".to_string(),
                tenant_namespace: Some("tenant-b".to_string()),
                scopes: vec!["tasks:manage".to_string()],
            },
        ]));

    let state = AppState::new(manager.clone(), resolver, auth);

    // Create a task for tenant-a.
    let tenant_a = tenants.get("tenant-a").unwrap();
    let handle = manager
        .create_task(tenant_a, "task for a", None, serde_json::json!({}), None)
        .unwrap();

    let router = build_router(state, vec![], None);
    let server = axum_test::TestServer::new(router).unwrap();

    // tenant-b tries to cancel tenant-a's task → must get 403.
    let response = server
        .post(&format!("/api/v1/tasks/{}/cancel", handle.task_id))
        .add_header(
            axum::http::HeaderName::from_static("authorization"),
            axum::http::HeaderValue::from_static("ApiKey key-b"),
        )
        .await;

    response.assert_status(axum::http::StatusCode::FORBIDDEN);

    // Task must still be in Running state (not cancelled).
    let task = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(
        task.state,
        TaskState::Running,
        "task must remain Running after unauthorized cancel attempt"
    );
}

// ─── BLOCKER 5: Webhook route mounting ────────────────────────────────────────

// TODO(S032): the production webhook route now goes through
// `engine.spawn_task`, which requires both the `csm` tenant AND the
// `csm-agent` agent_type to be seeded in the engine's catalog. The
// `AppState::with_webhooks` constructor builds an empty in-memory catalog,
// and only `tenants_repo()` is exposed publicly — there is no seam to seed
// the agent row from the test. Re-enable once the test harness can build an
// AppState that combines a pre-seeded engine with a webhook config (e.g. a
// `with_engine_and_webhooks` constructor or by wiring the test through
// engine_catalog-style fixtures).
#[ignore = "S032: webhook route now requires seeded engine catalog (tenant + agent); test harness needs an AppState constructor that combines `with_engine` + webhooks"]
#[tokio::test]
async fn webhook_route_is_mounted_and_responds_to_post() {
    use simulacra_server::{
        AppState, TaskManager, TenantConfig, TenantResolver, WebhookConfig,
        auth::CompositeAuthProvider, build_router, compute_hmac_signature,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    const SECRET_VAR: &str = "SIMULACRA_TEST_WH_ROUTE";
    const SECRET_VAL: &str = "route-test-secret";
    // SAFETY: test env manipulation.
    unsafe { std::env::set_var(SECRET_VAR, SECRET_VAL) };

    let mut tenants = HashMap::new();
    tenants.insert(
        "csm".to_string(),
        TenantConfig {
            namespace: "csm".to_string(),
            agent_type: "csm-agent".to_string(),
            vfs_root: PathBuf::from("/data/csm"),
            budget_pool: BudgetPoolConfig::default(),
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );

    let manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::new(tenants, None));
    let auth: Arc<dyn simulacra_server::AuthProvider> =
        Arc::new(CompositeAuthProvider::new(vec![]));

    let webhook = WebhookConfig {
        name: "test-webhook".to_string(),
        path: "/hooks/test".to_string(),
        tenant: "csm".to_string(),
        task_template: "Handle event: {{payload.event}}".to_string(),
        agent_type: "csm-agent".to_string(),
        secret: SECRET_VAR.to_string(),
    };

    let state = AppState::with_webhooks(manager, resolver, auth, vec![webhook]);
    let router = build_router(state, vec![], None);
    let server = axum_test::TestServer::new(router).unwrap();

    let body = br#"{"event": "new-signup"}"#;
    let sig = compute_hmac_signature(SECRET_VAL, body);

    let response = server
        .post("/hooks/test")
        .add_header(
            axum::http::HeaderName::from_static("x-simulacra-signature"),
            axum::http::HeaderValue::from_str(&sig).unwrap(),
        )
        .bytes(body.as_ref().into())
        .await;

    response.assert_status_ok();
    let json: serde_json::Value = response.json();
    assert!(
        json.get("task_id").is_some(),
        "webhook POST must return task_id"
    );
}

// ─── BLOCKER 1: SSE stream closes after terminal state ────────────────────────

#[test]
fn sse_stream_closes_after_task_reaches_terminal_state() {
    use simulacra_server::{BudgetPoolConfig, TaskManager, TaskState, TenantConfig};
    use std::path::PathBuf;

    let manager = TaskManager::new();
    let tenant = TenantConfig {
        namespace: "csm".to_string(),
        agent_type: "csm-agent".to_string(),
        vfs_root: PathBuf::from("/data/csm"),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    };

    // Create a task and subscribe before cancelling.
    let handle = manager
        .create_task(&tenant, "test task", None, serde_json::json!({}), None)
        .unwrap();

    let (_history, mut rx) = manager.subscribe_task(&handle.task_id).unwrap();

    // Cancel the task — this emits a task.state_changed event with to: "cancelled".
    manager.cancel_task(&handle.task_id).unwrap();

    // Drain the channel: must find a terminal event.
    let mut found_terminal = false;
    while let Ok(event) = rx.try_recv() {
        let to = event.get("to").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(to, "completed" | "failed" | "killed" | "cancelled") {
            found_terminal = true;
            break;
        }
    }

    assert!(
        found_terminal,
        "SSE channel must emit a terminal task.state_changed event after cancel"
    );

    // After the terminal event, the task must be in a terminal state.
    let task = manager.get_task(&handle.task_id).unwrap();
    assert_eq!(
        task.state,
        TaskState::Cancelled,
        "task must be in Cancelled state after cancel"
    );

    // No more non-terminal events should arrive (channel may be empty or closed).
    let remaining: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    for ev in &remaining {
        let to = ev.get("to").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            !matches!(
                to,
                "running" | "streaming" | "paused" | "waiting_input" | "waiting_approval"
            ),
            "no non-terminal state events should follow the terminal event"
        );
    }
}
