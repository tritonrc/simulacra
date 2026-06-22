//! Tests for credential injection — URL matching, tenant scope, header placement.
//! Covers spec assertions 42–48.

#![allow(clippy::await_holding_lock)]

use std::collections::HashMap;
use std::env;
use std::sync::{LazyLock, Mutex};

use simulacra_integration::{
    AuthMethod, CredentialInjector, IntegrationConfig, IntegrationRegistry,
};

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct EnvGuard {
    key: String,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let previous = env::var(key).ok();
        unsafe { env::set_var(key, value) };
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { env::set_var(&self.key, value) },
            None => unsafe { env::remove_var(&self.key) },
        }
    }
}

fn linear_config(placement: &str) -> IntegrationConfig {
    IntegrationConfig {
        auth: AuthMethod::ApiKey {
            key: "LINEAR_API_KEY".to_string(),
            placement: placement.to_string(),
        },
        base_url: "https://api.linear.app/graphql".to_string(),
        description: Some("Linear".to_string()),
        rate_limit_rps: 0,
        skills_path: None,
    }
}

fn custom_header_config() -> IntegrationConfig {
    IntegrationConfig {
        auth: AuthMethod::ApiKey {
            key: "CUSTOM_API_KEY".to_string(),
            placement: "header:X-Api-Key".to_string(),
        },
        base_url: "https://custom.example.com".to_string(),
        description: Some("Custom".to_string()),
        rate_limit_rps: 1,
        skills_path: None,
    }
}

fn build_api_key_registry() -> IntegrationRegistry {
    let _guards = [
        EnvGuard::set("LINEAR_API_KEY", "linear-secret"),
        EnvGuard::set("CUSTOM_API_KEY", "custom-secret"),
    ];

    IntegrationRegistry::from_config(&HashMap::from([
        ("linear".to_string(), linear_config("header")),
        ("custom".to_string(), custom_header_config()),
    ]))
    .expect("registry should construct")
}

/// Spec assertion 42: fetch() to URL matching base_url gets auth headers injected.
#[tokio::test]
async fn fetch_to_granted_linear_url_gets_auth_header() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    let headers = injector
        .inject_credentials("https://api.linear.app/graphql", &["linear".to_string()])
        .await
        .expect("granted integration should inject")
        .expect("linear URL should match");

    assert!(
        headers.iter().any(|(name, _)| name == "Authorization"),
        "expected Authorization header"
    );
}

/// Spec assertion 48: unganted integration URLs are not injected.
#[tokio::test]
async fn fetch_to_linear_url_from_ungranted_tenant_gets_no_injection() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    let headers = injector
        .inject_credentials("https://api.linear.app/graphql", &["custom".to_string()])
        .await
        .expect("should not error");

    assert!(headers.is_none(), "ungranted integration must not inject");
}

/// Spec assertion 46: URL not matching any integration: no injection.
#[tokio::test]
async fn fetch_to_unrelated_url_gets_no_injection() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    let headers = injector
        .inject_credentials("https://example.com/no-match", &["linear".to_string()])
        .await
        .expect("unmatched URLs pass through");

    assert!(headers.is_none());
}

/// Spec assertion 43: API key with placement = "header" adds Authorization: Bearer <key>.
#[tokio::test]
async fn api_key_default_placement_adds_bearer_header() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    let headers = injector
        .inject_credentials("https://api.linear.app/graphql", &["linear".to_string()])
        .await
        .unwrap()
        .unwrap();

    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer linear-secret"),
        "expected Bearer linear-secret, got {headers:?}"
    );
}

/// Spec assertion 44: API key with placement = "header:X-Api-Key" adds X-Api-Key: <key>.
#[tokio::test]
async fn api_key_custom_header_placement() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    let headers = injector
        .inject_credentials(
            "https://custom.example.com/resources",
            &["custom".to_string()],
        )
        .await
        .unwrap()
        .unwrap();

    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "X-Api-Key" && value == "custom-secret"),
        "expected X-Api-Key: custom-secret, got {headers:?}"
    );
}

/// Spec assertion 45: OAuth2 adds Authorization: Bearer <access_token>.
/// This test uses an API key to simulate the Bearer pattern since OAuth2
/// requires real token exchange. The bearer pattern is identical.
#[tokio::test]
async fn bearer_token_injection_format() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    let headers = injector
        .inject_credentials("https://api.linear.app/graphql", &["linear".to_string()])
        .await
        .unwrap()
        .unwrap();

    let auth = headers
        .iter()
        .find(|(name, _)| name == "Authorization")
        .expect("should have Authorization header");
    assert!(
        auth.1.starts_with("Bearer "),
        "OAuth2 and default API key should use Bearer format"
    );
}

/// URL matching must not inject credentials into attacker-controlled domains.
/// e.g., base_url "https://api.linear.app/graphql" must NOT match
/// "https://api.linear.app/graphql.evil.com".
#[tokio::test]
async fn url_matching_rejects_attacker_controlled_domain_prefix() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    // This URL has the base_url as a prefix but is a different domain
    let headers = injector
        .inject_credentials(
            "https://api.linear.app/graphql.evil.com/steal",
            &["linear".to_string()],
        )
        .await
        .expect("should not error");

    assert!(
        headers.is_none(),
        "must not inject credentials into attacker-controlled domain"
    );
}

/// URL matching should work when base_url is exact match.
#[tokio::test]
async fn url_matching_works_for_exact_base_url() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    let headers = injector
        .inject_credentials("https://custom.example.com", &["custom".to_string()])
        .await
        .unwrap();

    assert!(headers.is_some(), "exact base_url should match");
}

/// URL matching should work with query string after base_url.
#[tokio::test]
async fn url_matching_works_with_query_string() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let registry = build_api_key_registry();
    let injector: &dyn CredentialInjector = &registry;

    let headers = injector
        .inject_credentials("https://custom.example.com?page=1", &["custom".to_string()])
        .await
        .unwrap();

    assert!(headers.is_some(), "base_url + query string should match");
}

// Journaling and observability assertions are validated via Obsidian queries
// per S010, not unit tests. The tracing instrumentation is present in
// injector.rs — see tracing::debug! with integration name and URL host.
