//! Tests for GET /api/v1/triggers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use simulacra_server::auth::AuthProvider;
use simulacra_server::scheduler::{MissedPolicy, ScheduleConfig};
use simulacra_server::webhook::WebhookConfig;
use simulacra_server::{
    AppState, BudgetPoolConfig, NoAuthProvider, TaskManager, TenantConfig, TenantResolver,
    build_router,
};
use tower::ServiceExt;

fn make_state(webhooks: Vec<WebhookConfig>, schedules: Vec<ScheduleConfig>) -> AppState {
    let task_manager = Arc::new(TaskManager::new());
    let mut tenants = HashMap::new();
    tenants.insert("default".to_string(), default_tenant());
    let resolver = Arc::new(TenantResolver::new(tenants, Some("default".to_string())));
    let auth: Arc<dyn AuthProvider> = Arc::new(NoAuthProvider::new("dev@local", "default"));

    AppState::with_triggers(task_manager, resolver, auth, webhooks, schedules)
}

fn default_tenant() -> TenantConfig {
    TenantConfig {
        namespace: "default".to_string(),
        agent_type: "triage".to_string(),
        vfs_root: PathBuf::from("/tmp/simulacra/default"),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

fn webhook(name: &str, path: &str, tenant: &str, agent_type: &str, secret: &str) -> WebhookConfig {
    WebhookConfig {
        name: name.to_string(),
        path: path.to_string(),
        tenant: tenant.to_string(),
        task_template: "Handle inbound trigger".to_string(),
        agent_type: agent_type.to_string(),
        secret: secret.to_string(),
    }
}

fn schedule(
    name: &str,
    cron: &str,
    tenant: &str,
    agent_type: &str,
    missed_policy: MissedPolicy,
) -> ScheduleConfig {
    ScheduleConfig {
        name: name.to_string(),
        cron: cron.to_string(),
        tenant: tenant.to_string(),
        task: "Run scheduled task".to_string(),
        agent_type: agent_type.to_string(),
        missed_policy,
        enabled: true,
    }
}

async fn get_json(uri: &str, state: AppState) -> (StatusCode, Value) {
    let response = build_router(state, vec![], None)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();

    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&body).unwrap_or_else(|err| {
        panic!(
            "expected JSON response for {uri}, got status {status} and body {:?}: {err}",
            String::from_utf8_lossy(&body),
        )
    });

    (status, json)
}

#[tokio::test]
async fn returns_all_triggers_for_tenant_when_no_filter() {
    let state = make_state(
        vec![webhook(
            "zendesk-webhook",
            "/hooks/zendesk",
            "default",
            "triage",
            "ZENDESK_WEBHOOK_SECRET",
        )],
        vec![schedule(
            "weekly-report",
            "0 9 * * 1",
            "default",
            "weekly-report",
            MissedPolicy::Skip,
        )],
    );

    let (status, body) = get_json("/api/v1/triggers", state).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["webhooks"].as_array().unwrap().len(), 1);
    assert_eq!(body["schedules"].as_array().unwrap().len(), 1);
    assert_eq!(body["webhooks"][0]["path"], "/hooks/zendesk");
    assert_eq!(body["schedules"][0]["cron"], "0 9 * * 1");
}

#[tokio::test]
async fn filters_by_agent_query_param() {
    let state = make_state(
        vec![
            webhook(
                "triage-webhook",
                "/hooks/triage",
                "default",
                "triage",
                "TRIAGE_WEBHOOK_SECRET",
            ),
            webhook(
                "other-webhook",
                "/hooks/other",
                "default",
                "other",
                "OTHER_WEBHOOK_SECRET",
            ),
        ],
        vec![
            schedule(
                "triage-schedule",
                "0 9 * * 1",
                "default",
                "triage",
                MissedPolicy::Skip,
            ),
            schedule(
                "other-schedule",
                "0 10 * * 1",
                "default",
                "other",
                MissedPolicy::RunOnce,
            ),
        ],
    );

    let (status, body) = get_json("/api/v1/triggers?agent=triage", state).await;

    assert_eq!(status, StatusCode::OK);

    let webhooks = body["webhooks"].as_array().unwrap();
    assert_eq!(webhooks.len(), 1);
    assert_eq!(webhooks[0]["agent_type"], "triage");
    assert_eq!(webhooks[0]["path"], "/hooks/triage");

    let schedules = body["schedules"].as_array().unwrap();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0]["agent_type"], "triage");
    assert_eq!(schedules[0]["cron"], "0 9 * * 1");
}

#[tokio::test]
async fn cross_tenant_triggers_are_filtered_out() {
    let state = make_state(
        vec![
            webhook(
                "mine-webhook",
                "/hooks/mine",
                "default",
                "triage",
                "MY_WEBHOOK_SECRET",
            ),
            webhook(
                "other-webhook",
                "/hooks/other-tenant",
                "other-tenant",
                "triage",
                "OTHER_TENANT_SECRET",
            ),
        ],
        vec![
            schedule(
                "mine-schedule",
                "0 8 * * 1",
                "default",
                "triage",
                MissedPolicy::Skip,
            ),
            schedule(
                "other-schedule",
                "0 12 * * 1",
                "other-tenant",
                "triage",
                MissedPolicy::Backfill,
            ),
        ],
    );

    let (status, body) = get_json("/api/v1/triggers", state).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["webhooks"].as_array().unwrap().len(), 1);
    assert_eq!(body["webhooks"][0]["path"], "/hooks/mine");
    assert_eq!(body["schedules"].as_array().unwrap().len(), 1);
    assert_eq!(body["schedules"][0]["cron"], "0 8 * * 1");
}

#[tokio::test]
async fn webhook_response_includes_hmac_presence_flag_not_secret() {
    let state = make_state(
        vec![webhook(
            "secret-webhook",
            "/hooks/secret",
            "default",
            "triage",
            "SECRET_WEBHOOK_ENV_VAR",
        )],
        vec![],
    );

    let (status, body) = get_json("/api/v1/triggers", state).await;

    assert_eq!(status, StatusCode::OK);

    let webhook = &body["webhooks"][0];
    assert_eq!(webhook["path"], "/hooks/secret");
    assert_eq!(webhook["agent_type"], "triage");
    assert_eq!(webhook["hmac"], Value::Bool(true));
    assert!(
        webhook.get("secret").is_none(),
        "response must not expose the secret env var name"
    );
}

#[tokio::test]
async fn schedule_response_includes_cron_and_missed_policy() {
    let state = make_state(
        vec![],
        vec![schedule(
            "daily-run",
            "15 6 * * *",
            "default",
            "triage",
            MissedPolicy::RunOnce,
        )],
    );

    let (status, body) = get_json("/api/v1/triggers", state).await;

    assert_eq!(status, StatusCode::OK);

    let schedule = &body["schedules"][0];
    assert_eq!(schedule["agent_type"], "triage");
    assert_eq!(schedule["cron"], "15 6 * * *");
    assert_eq!(schedule["missed_policy"], "run-once");
    assert!(
        schedule.get("last_fire").is_none(),
        "response must not expose internal scheduler state"
    );
}
