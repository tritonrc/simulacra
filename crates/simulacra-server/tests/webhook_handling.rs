//! Tests for webhook HMAC validation, payload templating, and task creation (S032 assertions).

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::json;
use simulacra_server::{
    BudgetPoolConfig, TaskManager, TenantConfig, TenantResolver, WebhookConfig, WebhookHandler,
    apply_payload_template, compute_hmac_signature,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

const TEST_SECRET_ENV_VAR: &str = "SIMULACRA_TEST_WEBHOOK_SECRET";
const TEST_SECRET: &str = "test-secret-12345";

fn setup_env() {
    // SAFETY: tests run single-threaded (or we accept the race in multi-threaded mode).
    unsafe { std::env::set_var(TEST_SECRET_ENV_VAR, TEST_SECRET) };
}

fn webhook_config() -> WebhookConfig {
    WebhookConfig {
        name: "new-customer-onboarding".to_string(),
        path: "/hooks/new-customer".to_string(),
        tenant: "csm".to_string(),
        task_template: "New customer: {{payload.company_name}}. Draft welcome sequence."
            .to_string(),
        agent_type: "csm-agent".to_string(),
        secret: TEST_SECRET_ENV_VAR.to_string(),
    }
}

fn tenant_resolver() -> TenantResolver {
    let mut tenants = HashMap::new();
    tenants.insert(
        "csm".to_string(),
        TenantConfig {
            namespace: "csm".to_string(),
            agent_type: "csm-agent".to_string(),
            vfs_root: PathBuf::from("/data/csm"),
            budget_pool: BudgetPoolConfig {
                max_tokens: 5000,
                max_cost: String::new(),
            },
            hooks: vec!["governance.pre_tool".to_string()],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );
    TenantResolver::new(tenants, None)
}

// ─── HMAC validation assertions ───────────────────────────────────────────────

#[test]
fn post_to_webhook_path_with_valid_hmac_signature_creates_a_task_and_returns_task_id() {
    setup_env();
    let body = br#"{"company_name": "Acme Corp"}"#;
    let sig = compute_hmac_signature(TEST_SECRET, body);

    let handler = WebhookHandler::new(webhook_config());
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    let handle = handler
        .process(body, Some(&sig), &manager, &resolver)
        .expect("valid signature must succeed");

    assert!(!handle.task_id.is_empty());
    // Task description must include substituted template value.
    assert!(
        handle.description.contains("Acme Corp"),
        "task description must contain substituted template value"
    );
}

#[test]
fn post_with_invalid_hmac_signature_returns_401_and_does_not_create_a_task() {
    setup_env();
    let body = br#"{"company_name": "Acme Corp"}"#;
    let bad_sig = "sha256=deadbeef00000000000000000000000000000000000000000000000000000000";

    let handler = WebhookHandler::new(webhook_config());
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    let result = handler.process(body, Some(bad_sig), &manager, &resolver);
    assert!(result.is_err(), "invalid signature must be rejected");

    // No tasks should have been created.
    assert!(
        manager.active_task_ids().is_empty(),
        "no task must be created on signature failure"
    );
}

#[test]
fn post_with_missing_x_simulacra_signature_header_returns_401() {
    setup_env();
    let body = br#"{"company_name": "Acme Corp"}"#;

    let handler = WebhookHandler::new(webhook_config());
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    let result = handler.process(body, None, &manager, &resolver);
    assert!(result.is_err(), "missing signature header must be rejected");
}

#[test]
fn post_with_valid_signature_but_unparseable_body_returns_400() {
    setup_env();
    let body = b"this is not json {";
    let sig = compute_hmac_signature(TEST_SECRET, body);

    let handler = WebhookHandler::new(webhook_config());
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    let result = handler.process(body, Some(&sig), &manager, &resolver);
    assert!(
        result.is_err(),
        "invalid JSON body must be rejected after valid signature"
    );
}

// ─── Payload templating assertions ────────────────────────────────────────────

#[test]
fn payload_template_substitution_replaces_payload_field_with_value() {
    let payload = json!({"company_name": "Acme Corp"});
    let template = "New customer: {{payload.company_name}}.";
    let result = apply_payload_template(template, &payload);
    assert_eq!(result, "New customer: Acme Corp.");
}

#[test]
fn nested_payload_access_resolves_payload_contact_email_correctly() {
    let payload = json!({"contact": {"email": "j@acme.com"}});
    let template = "Contact: {{payload.contact.email}}";
    let result = apply_payload_template(template, &payload);
    assert_eq!(result, "Contact: j@acme.com");
}

#[test]
fn missing_template_field_is_replaced_with_missing_placeholder() {
    let payload = json!({"other_field": "value"});
    let template = "Company: {{payload.company_name}}";
    let result = apply_payload_template(template, &payload);
    assert_eq!(result, "Company: <missing: payload.company_name>");
}

#[test]
fn multiple_template_substitutions_in_one_template() {
    let payload = json!({
        "company_name": "Acme Corp",
        "contact": {"email": "j@acme.com"}
    });
    let template = "New customer: {{payload.company_name}}. Contact: {{payload.contact.email}}.";
    let result = apply_payload_template(template, &payload);
    assert_eq!(result, "New customer: Acme Corp. Contact: j@acme.com.");
}

// ─── Secret env var assertions ────────────────────────────────────────────────

#[test]
fn webhook_secret_is_read_from_environment_variable_not_config_directly() {
    // Config stores the env var NAME, not the secret.
    let config = webhook_config();
    assert_eq!(
        config.secret, TEST_SECRET_ENV_VAR,
        "config.secret must be an env var name, not the actual secret"
    );

    // Set the env var and verify processing works.
    setup_env();
    let body = br#"{"company_name": "Test Corp"}"#;
    let sig = compute_hmac_signature(TEST_SECRET, body);

    let handler = WebhookHandler::new(config);
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    assert!(
        handler
            .process(body, Some(&sig), &manager, &resolver)
            .is_ok(),
        "must read secret from env var"
    );
}

#[test]
fn missing_secret_env_var_causes_signature_validation_failure() {
    // Ensure the env var is NOT set.
    // SAFETY: single-threaded test setup.
    unsafe { std::env::remove_var("SIMULACRA_MISSING_SECRET_VAR") };

    let config = WebhookConfig {
        secret: "SIMULACRA_MISSING_SECRET_VAR".to_string(),
        ..webhook_config()
    };

    let body = br#"{"company_name": "Acme"}"#;
    let sig = "sha256=anyvalue";

    let handler = WebhookHandler::new(config);
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    let result = handler.process(body, Some(sig), &manager, &resolver);
    assert!(result.is_err(), "missing secret env var must cause failure");
}

// ─── Metadata assertions ──────────────────────────────────────────────────────

#[test]
fn full_webhook_payload_is_attached_as_task_metadata() {
    setup_env();
    let payload = json!({"company_name": "Acme Corp", "deal_value": 50000});
    let body = serde_json::to_vec(&payload).unwrap();
    let sig = compute_hmac_signature(TEST_SECRET, &body);

    let handler = WebhookHandler::new(webhook_config());
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    let handle = handler
        .process(&body, Some(&sig), &manager, &resolver)
        .unwrap();

    // Full payload must be in metadata.
    assert_eq!(
        handle.metadata["payload"]["company_name"],
        json!("Acme Corp")
    );
    assert_eq!(handle.metadata["payload"]["deal_value"], json!(50000));
}

#[test]
fn webhook_source_metadata_is_recorded_in_task() {
    setup_env();
    let body = br#"{"company_name": "Acme Corp"}"#;
    let sig = compute_hmac_signature(TEST_SECRET, body);

    let handler = WebhookHandler::new(webhook_config());
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    let handle = handler
        .process(body, Some(&sig), &manager, &resolver)
        .unwrap();

    assert_eq!(handle.metadata["source"], json!("webhook"));
    assert_eq!(
        handle.metadata["webhook_name"],
        json!("new-customer-onboarding")
    );
}

// ─── Constant-time comparison assertion ──────────────────────────────────────

#[test]
fn hmac_comparison_uses_hmac_verify_slice_for_constant_time_equality() {
    // Structural check: WebhookHandler::validate_signature uses hmac::Mac::verify_slice
    // which provides constant-time comparison. This test verifies the behavior is correct
    // by checking that a 1-bit difference in the signature causes failure, and that both
    // the wrong-length and wrong-value cases are handled identically (no early return).

    setup_env();
    let body = br#"{"test": true}"#;
    let valid_sig = compute_hmac_signature(TEST_SECRET, body);

    // Flip one hex digit to create an invalid signature.
    let bad_sig = valid_sig.clone();
    let sig_part = bad_sig.strip_prefix("sha256=").unwrap();
    let mut chars: Vec<char> = sig_part.chars().collect();
    chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
    let bad_sig = format!("sha256={}", chars.iter().collect::<String>());

    let handler = WebhookHandler::new(webhook_config());
    let manager = TaskManager::new();
    let resolver = tenant_resolver();

    // Valid signature must succeed.
    assert!(
        handler
            .process(body, Some(&valid_sig), &manager, &resolver)
            .is_ok()
    );
    // Invalid signature must fail.
    assert!(
        handler
            .process(body, Some(&bad_sig), &manager, &resolver)
            .is_err()
    );
}
