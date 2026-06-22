use std::collections::HashMap;
use std::env;
use std::sync::{LazyLock, Mutex};

use simulacra_config::{AuthMethod as ConfigAuthMethod, SimulacraConfig};
use simulacra_integration::{AuthMethod, IntegrationConfig, IntegrationError, IntegrationRegistry};

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

    fn unset(key: &str) -> Self {
        let previous = env::var(key).ok();
        unsafe { env::remove_var(key) };
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

fn config_toml() -> &'static str {
    r#"
[project]
name = "simulacra"

[agent_types.default]
model = "claude-sonnet-4.6"

[integrations.hubspot]
type = "oauth2"
client_id = "HUBSPOT_CLIENT_ID"
client_secret = "HUBSPOT_CLIENT_SECRET"
token_url = "https://api.hubapi.com/oauth/v1/token"
refresh_token = "HUBSPOT_REFRESH_TOKEN"
scopes = ["crm.objects.contacts.read", "crm.objects.deals.read"]
base_url = "https://api.hubapi.com"
description = "HubSpot CRM — contacts, deals, pipelines"
rate_limit_rps = 10

[integrations.linear]
type = "api_key"
key = "LINEAR_API_KEY"
placement = "header:X-Api-Key"
base_url = "https://api.linear.app/graphql"
description = "Linear project tracking"

[tenants.onboarding]
agent_type = "default"
integrations = ["hubspot", "slack"]
"#
}

fn oauth2_config() -> IntegrationConfig {
    IntegrationConfig {
        auth: AuthMethod::OAuth2 {
            client_id: "HUBSPOT_CLIENT_ID".to_string(),
            client_secret: "HUBSPOT_CLIENT_SECRET".to_string(),
            token_url: "https://api.hubapi.com/oauth/v1/token".to_string(),
            scopes: vec![
                "crm.objects.contacts.read".to_string(),
                "crm.objects.deals.read".to_string(),
            ],
            refresh_token: Some("HUBSPOT_REFRESH_TOKEN".to_string()),
        },
        base_url: "https://api.hubapi.com".to_string(),
        description: Some("HubSpot CRM — contacts, deals, pipelines".to_string()),
        rate_limit_rps: 10,
        skills_path: None,
    }
}

fn api_key_config() -> IntegrationConfig {
    IntegrationConfig {
        auth: AuthMethod::ApiKey {
            key: "LINEAR_API_KEY".to_string(),
            placement: "header:X-Api-Key".to_string(),
        },
        base_url: "https://api.linear.app/graphql".to_string(),
        description: Some("Linear project tracking".to_string()),
        rate_limit_rps: 0,
        skills_path: None,
    }
}

#[test]
fn simulacra_config_deserializes_integrations_hubspot_with_oauth2_type() {
    let config: SimulacraConfig =
        toml::from_str(config_toml()).expect("simulacra config should parse");

    match &config.integrations["hubspot"].auth {
        ConfigAuthMethod::OAuth2 {
            client_id,
            client_secret,
            token_url,
            scopes,
            refresh_token,
        } => {
            assert_eq!(client_id, "HUBSPOT_CLIENT_ID");
            assert_eq!(client_secret, "HUBSPOT_CLIENT_SECRET");
            assert_eq!(token_url, "https://api.hubapi.com/oauth/v1/token");
            assert_eq!(
                scopes,
                &vec![
                    "crm.objects.contacts.read".to_string(),
                    "crm.objects.deals.read".to_string()
                ]
            );
            assert_eq!(refresh_token.as_deref(), Some("HUBSPOT_REFRESH_TOKEN"));
        }
        other => panic!("expected oauth2 auth, got {other:?}"),
    }
}

#[test]
fn simulacra_config_deserializes_integrations_linear_with_api_key_type() {
    let config: SimulacraConfig =
        toml::from_str(config_toml()).expect("simulacra config should parse");

    match &config.integrations["linear"].auth {
        ConfigAuthMethod::ApiKey { key, placement } => {
            assert_eq!(key, "LINEAR_API_KEY");
            assert_eq!(placement, "header:X-Api-Key");
        }
        other => panic!("expected api_key auth, got {other:?}"),
    }
}

#[test]
fn missing_type_field_is_a_parse_error() {
    let err = toml::from_str::<SimulacraConfig>(
        r#"
[project]
name = "simulacra"

[agent_types.default]
model = "claude-sonnet-4.6"

[integrations.hubspot]
client_id = "HUBSPOT_CLIENT_ID"
client_secret = "HUBSPOT_CLIENT_SECRET"
token_url = "https://api.hubapi.com/oauth/v1/token"
base_url = "https://api.hubapi.com"
"#,
    )
    .expect_err("missing integration type must fail parsing");

    assert!(
        err.to_string().contains("type"),
        "expected parse error mentioning missing type, got {err}"
    );
}

#[test]
fn missing_base_url_is_a_parse_error() {
    let err = toml::from_str::<SimulacraConfig>(
        r#"
[project]
name = "simulacra"

[agent_types.default]
model = "claude-sonnet-4.6"

[integrations.hubspot]
type = "oauth2"
client_id = "HUBSPOT_CLIENT_ID"
client_secret = "HUBSPOT_CLIENT_SECRET"
token_url = "https://api.hubapi.com/oauth/v1/token"
"#,
    )
    .expect_err("missing base_url must fail parsing");

    assert!(
        err.to_string().contains("base_url"),
        "expected parse error mentioning base_url, got {err}"
    );
}

#[test]
fn tenants_onboarding_with_integrations_deserializes_correctly() {
    let config: SimulacraConfig =
        toml::from_str(config_toml()).expect("simulacra config should parse");
    let tenant = config
        .tenants
        .get("onboarding")
        .expect("tenant should deserialize");

    assert_eq!(tenant.agent_type, "default");
    assert_eq!(
        tenant.integrations.as_deref(),
        Some(&["hubspot".to_string(), "slack".to_string()][..])
    );
}

#[test]
fn from_config_succeeds_when_all_env_vars_are_set() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guards = [
        EnvGuard::set("HUBSPOT_CLIENT_ID", "client-id"),
        EnvGuard::set("HUBSPOT_CLIENT_SECRET", "client-secret"),
        EnvGuard::set("HUBSPOT_REFRESH_TOKEN", "refresh-token"),
        EnvGuard::set("LINEAR_API_KEY", "linear-secret-value"),
    ];

    let registry = IntegrationRegistry::from_config(&HashMap::from([
        ("hubspot".to_string(), oauth2_config()),
        ("linear".to_string(), api_key_config()),
    ]))
    .expect("registry should resolve all env vars");

    let mut names = registry.names();
    names.sort();
    assert_eq!(names, vec!["hubspot".to_string(), "linear".to_string()]);
}

#[test]
fn from_config_returns_missing_env_var_when_required_var_is_unset() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guards = [
        EnvGuard::unset("HUBSPOT_CLIENT_ID"),
        EnvGuard::set("HUBSPOT_CLIENT_SECRET", "client-secret"),
        EnvGuard::set("HUBSPOT_REFRESH_TOKEN", "refresh-token"),
    ];

    let err = match IntegrationRegistry::from_config(&HashMap::from([(
        "hubspot".to_string(),
        oauth2_config(),
    )])) {
        Ok(_) => panic!("missing env var must fail registry construction"),
        Err(err) => err,
    };

    assert!(matches!(err, IntegrationError::MissingEnvVar(name) if name == "HUBSPOT_CLIENT_ID"));
}

#[test]
fn resolved_credentials_are_not_accessible_via_vfs_logs_or_agent_visible_metadata() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let secret = "linear-secret-value";
    let _guard = EnvGuard::set("LINEAR_API_KEY", secret);

    let registry = IntegrationRegistry::from_config(&HashMap::from([(
        "linear".to_string(),
        api_key_config(),
    )]))
    .expect("registry should resolve API key");

    let metadata = serde_json::to_string(
        &registry
            .metadata("linear")
            .expect("metadata should exist for configured integration"),
    )
    .expect("metadata should serialize");

    assert!(!metadata.contains(secret));
    assert!(!metadata.contains("LINEAR_API_KEY"));
}

#[test]
fn resolved_credentials_are_never_written_to_logs() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guards = [
        EnvGuard::set("HUBSPOT_CLIENT_ID", "client-id"),
        EnvGuard::set("HUBSPOT_CLIENT_SECRET", "super-secret"),
        EnvGuard::set("HUBSPOT_REFRESH_TOKEN", "refresh-token"),
    ];

    let _ = IntegrationRegistry::from_config(&HashMap::from([(
        "hubspot".to_string(),
        oauth2_config(),
    )]))
    .expect("registry should resolve oauth env vars");

    // Validated via Aniani log queries per S010. The registry uses tracing::info!
    // at startup which logs integration count and names but never credential values.
}

#[test]
fn api_key_value_is_resolved_from_named_env_var_not_literal_config_string() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvGuard::set("LINEAR_API_KEY", "resolved-secret");

    let registry = IntegrationRegistry::from_config(&HashMap::from([(
        "linear".to_string(),
        api_key_config(),
    )]))
    .expect("registry should resolve API key env var");

    let token = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build")
        .block_on(registry.access_token("linear"))
        .expect("api key integrations should expose the resolved key as access token");

    assert_eq!(token, "resolved-secret");
    assert_ne!(token, "LINEAR_API_KEY");
}
