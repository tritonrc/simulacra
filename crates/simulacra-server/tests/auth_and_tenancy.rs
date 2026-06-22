//! Tests for authentication (OIDC, API keys) and tenant resolution (S031 assertions).

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::json;
use simulacra_server::{
    ApiKeyAuthProvider, ApiKeyEntry, AuthError, AuthProvider, BudgetPoolConfig, Credentials,
    Identity, TenantConfig, TenantError, TenantResolver,
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn tenant(namespace: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: format!("{namespace}-agent"),
        vfs_root: PathBuf::from(format!("/srv/{namespace}")),
        budget_pool: BudgetPoolConfig {
            max_tokens: 5000,
            max_cost: String::new(),
        },
        hooks: vec!["hook.audit".to_string()],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

fn resolver_with_default(default_tenant: Option<&str>) -> TenantResolver {
    let mut tenants = HashMap::new();
    tenants.insert("accounting".to_string(), tenant("accounting"));
    tenants.insert("csm".to_string(), tenant("csm"));
    TenantResolver::new(tenants, default_tenant.map(str::to_string))
}

fn identity(tenant_namespace: Option<&str>) -> Identity {
    Identity {
        subject: "user-1".to_string(),
        tenant_namespace: tenant_namespace.map(str::to_string),
        scopes: vec![],
        metadata: json!({}),
    }
}

fn api_key_provider() -> ApiKeyAuthProvider {
    ApiKeyAuthProvider::from_entries(vec![
        ApiKeyEntry {
            key: "svc-key-1".to_string(),
            subject: "svc-csm".to_string(),
            tenant_namespace: Some("csm".to_string()),
            scopes: vec!["tasks:create".into()],
        },
        ApiKeyEntry {
            key: "svc-key-2".to_string(),
            subject: "svc-accounting".to_string(),
            tenant_namespace: Some("accounting".to_string()),
            scopes: vec!["tasks:create".into()],
        },
    ])
}

// ─── API key auth assertions ───────────────────────────────────────────────────

#[tokio::test]
async fn valid_api_key_returns_identity_with_subject_and_tenant_namespace_from_key_metadata() {
    let provider = api_key_provider();
    let identity = provider
        .authenticate(&Credentials::ApiKey("svc-key-1".to_string()))
        .await
        .expect("valid API key must authenticate");

    assert_eq!(identity.subject, "svc-csm");
    assert_eq!(identity.tenant_namespace.as_deref(), Some("csm"));
}

#[tokio::test]
async fn unknown_api_key_returns_unauthorized() {
    let provider = api_key_provider();
    let result = provider
        .authenticate(&Credentials::ApiKey("unknown-key".to_string()))
        .await;

    assert!(
        matches!(result, Err(AuthError::Unauthorized)),
        "unknown key must return Unauthorized"
    );
}

#[tokio::test]
async fn empty_api_key_returns_missing_credentials() {
    let provider = api_key_provider();
    let result = provider
        .authenticate(&Credentials::ApiKey(String::new()))
        .await;

    assert!(
        matches!(result, Err(AuthError::MissingCredentials)),
        "empty key must return MissingCredentials"
    );
}

#[tokio::test]
async fn api_key_provider_rejects_bearer_credentials() {
    let provider = api_key_provider();
    let result = provider
        .authenticate(&Credentials::Bearer("some-token".to_string()))
        .await;

    assert!(
        matches!(result, Err(AuthError::Unauthorized)),
        "API key provider must reject Bearer credentials"
    );
}

// ─── OIDC auth assertions ──────────────────────────────────────────────────────

#[tokio::test]
async fn oidc_provider_rejects_empty_bearer_token_as_missing_credentials() {
    use simulacra_server::{OidcAuthProvider, OidcConfig};

    let config = OidcConfig {
        issuer: "https://example.okta.com".to_string(),
        audience: "simulacra-api".to_string(),
        tenant_claim: "org.department".to_string(),
        jwks_url: None,
    };
    let provider =
        OidcAuthProvider::new(config).expect("OIDC provider without jwks_url must construct");

    let result = provider
        .authenticate(&Credentials::Bearer(String::new()))
        .await;

    assert!(
        matches!(result, Err(AuthError::MissingCredentials)),
        "empty Bearer token must return MissingCredentials"
    );
}

#[tokio::test]
async fn oidc_provider_with_no_jwks_rejects_any_token() {
    use simulacra_server::{OidcAuthProvider, OidcConfig};

    let config = OidcConfig {
        issuer: "https://unreachable.example.com".to_string(),
        audience: "simulacra-api".to_string(),
        tenant_claim: "org.department".to_string(),
        jwks_url: None,
    };
    // Provider initialized without JWKS URL — every token is rejected.
    let provider =
        OidcAuthProvider::new(config).expect("provider must construct when jwks_url is None");

    let result = provider
        .authenticate(&Credentials::Bearer("valid.looking.token".to_string()))
        .await;

    // Should fail — no JWKS available yet.
    assert!(
        result.is_err(),
        "provider with no JWKS must reject all tokens"
    );
}

// ─── Tenant resolver assertions ────────────────────────────────────────────────

#[test]
fn tenant_resolver_maps_identity_with_tenant_namespace_accounting_to_accounting_config() {
    let resolver = resolver_with_default(None);
    let id = identity(Some("accounting"));

    let resolved = resolver
        .resolve(&id)
        .expect("tenant must resolve with valid hint");
    assert_eq!(resolved.namespace, "accounting");
}

#[test]
fn identity_with_no_tenant_namespace_and_no_default_tenant_returns_forbidden() {
    let resolver = resolver_with_default(None);
    let id = identity(None);

    let result = resolver.resolve(&id);
    assert!(
        matches!(result, Err(TenantError::Forbidden)),
        "no hint + no default must return Forbidden"
    );
}

#[test]
fn identity_with_no_tenant_namespace_and_a_default_tenant_resolves_to_the_default() {
    let resolver = resolver_with_default(Some("csm"));
    let id = identity(None);

    let resolved = resolver
        .resolve(&id)
        .expect("default tenant must resolve when no hint");
    assert_eq!(resolved.namespace, "csm");
}

#[test]
fn identity_with_nonexistent_tenant_namespace_returns_forbidden() {
    let resolver = resolver_with_default(None);
    let id = identity(Some("does-not-exist"));

    let result = resolver.resolve(&id);
    assert!(
        matches!(result, Err(TenantError::Forbidden)),
        "unknown tenant hint must return Forbidden"
    );
}

#[test]
fn tenant_namespace_takes_priority_over_default_tenant() {
    // Default is "csm" but hint says "accounting" — hint must win.
    let resolver = resolver_with_default(Some("csm"));
    let id = identity(Some("accounting"));

    let resolved = resolver.resolve(&id).expect("hint must override default");
    assert_eq!(resolved.namespace, "accounting");
}

// ─── Tenant validation assertions ─────────────────────────────────────────────

#[test]
fn tenant_config_validate_rejects_empty_agent_type() {
    let t = TenantConfig {
        namespace: "bad".to_string(),
        agent_type: String::new(), // invalid
        vfs_root: PathBuf::from("/data/bad"),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    };
    assert!(
        t.validate().is_err(),
        "empty agent_type must fail validation"
    );
}

#[test]
fn tenant_config_validate_rejects_empty_vfs_root() {
    let t = TenantConfig {
        namespace: "bad".to_string(),
        agent_type: "some-agent".to_string(),
        vfs_root: PathBuf::from(""), // invalid
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    };
    assert!(t.validate().is_err(), "empty vfs_root must fail validation");
}

#[test]
fn tenant_resolver_validate_all_returns_errors_for_invalid_configs() {
    let mut tenants = HashMap::new();
    tenants.insert(
        "bad".to_string(),
        TenantConfig {
            namespace: "bad".to_string(),
            agent_type: String::new(), // will fail
            vfs_root: PathBuf::from("/data/bad"),
            budget_pool: BudgetPoolConfig::default(),
            hooks: vec![],
            integrations: vec![],
            mcp_servers: Default::default(),
        },
    );
    let resolver = TenantResolver::new(tenants, None);
    let errors = resolver.validate_all();
    assert!(
        !errors.is_empty(),
        "invalid tenant config must surface validation errors"
    );
}
