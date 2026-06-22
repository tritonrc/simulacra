//! Authentication providers: OIDC and API key.

use std::collections::HashMap;

use async_trait::async_trait;
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tracing::{instrument, warn};

use crate::metrics::ServerMeters;

fn auth_error_reason(err: &AuthError) -> &'static str {
    match err {
        AuthError::Unauthorized => "unauthorized",
        AuthError::Expired => "expired",
        AuthError::InvalidSignature => "invalid_signature",
        AuthError::MissingCredentials => "missing_credentials",
        AuthError::Other(_) => "other",
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────────────────────

/// Authenticated identity returned by an `AuthProvider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// User or service principal subject identifier.
    pub subject: String,
    /// Tenant namespace extracted from OIDC claim or API key metadata.
    pub tenant_namespace: Option<String>,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Provider-specific metadata.
    pub metadata: Value,
}

/// Credentials presented by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Credentials {
    /// OIDC Bearer JWT token.
    Bearer(String),
    /// Service-to-service API key.
    ApiKey(String),
}

/// Errors returned by `AuthProvider::authenticate`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    #[error("unauthorized")]
    Unauthorized,

    #[error("token expired")]
    Expired,

    #[error("invalid signature")]
    InvalidSignature,

    #[error("missing credentials")]
    MissingCredentials,

    #[error("auth error: {0}")]
    Other(String),
}

/// Authentication provider trait.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Validate credentials and return an authenticated identity.
    async fn authenticate(&self, credentials: &Credentials) -> Result<Identity, AuthError>;
}

/// Fetch a JWKS from `url` using a synchronous `ureq` agent and return the
/// first usable decoding key. Any error (network, non-200, malformed JSON,
/// no RSA/EC key present) is mapped to `AuthError::Other`.
///
/// This runs synchronously inside `OidcAuthProvider::new`. It is only called
/// at startup, before the tokio runtime is servicing requests, so blocking is
/// acceptable here — the alternative (constructing a broken provider and
/// hoping someone notices) is what BLOCKER 1 fixes.
fn fetch_first_jwks_decoding_key(url: &str) -> Result<jsonwebtoken::DecodingKey, AuthError> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(AuthError::Other(format!(
            "OIDC jwks_url must be http(s), got {url}"
        )));
    }

    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .new_agent();

    let response = agent
        .get(url)
        .call()
        .map_err(|e| AuthError::Other(format!("OIDC JWKS fetch failed for {url}: {e}")))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(AuthError::Other(format!(
            "OIDC JWKS endpoint {url} returned status {status}"
        )));
    }
    let body = response
        .into_body()
        .read_to_vec()
        .map_err(|e| AuthError::Other(format!("failed to read OIDC JWKS body: {e}")))?;

    let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_slice(&body)
        .map_err(|e| AuthError::Other(format!("malformed OIDC JWKS: {e}")))?;

    // Pick the first key we can actually build a DecodingKey from.
    for jwk in &jwks.keys {
        if let Ok(key) = jsonwebtoken::DecodingKey::from_jwk(jwk) {
            return Ok(key);
        }
    }
    Err(AuthError::Other(format!(
        "OIDC JWKS at {url} contains no usable keys"
    )))
}

// ──────────────────────────────────────────────────────────────────────────────
// OIDC auth provider
// ──────────────────────────────────────────────────────────────────────────────

/// Configuration for the OIDC authentication provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
    /// OIDC claim path for tenant extraction (e.g. "org.department").
    pub tenant_claim: String,
    /// Optional JWKS endpoint. When set, `OidcAuthProvider::new` will attempt to
    /// fetch and parse the JWKS at startup and fail hard if either step fails.
    /// When `None`, the provider is constructed without a decoding key and
    /// every `authenticate` call rejects the token — used by tests and for
    /// bring-up when JWKS endpoints are not yet reachable.
    #[serde(default)]
    pub jwks_url: Option<String>,
}

/// OIDC-based JWT authentication provider.
///
/// Validates Bearer tokens against the configured OIDC issuer using `jsonwebtoken`.
/// If `jwks_url` is configured, `new()` fetches the JWKS and fails hard on any
/// error — misconfigured servers should not silently break every request. If
/// `jwks_url` is unset, a warning is logged and the provider rejects every token.
pub struct OidcAuthProvider {
    config: OidcConfig,
    /// Decoding key loaded from OIDC discovery. None if no JWKS URL was configured.
    decoding_key: Option<jsonwebtoken::DecodingKey>,
}

impl OidcAuthProvider {
    /// Create an OIDC auth provider from config.
    ///
    /// When `config.jwks_url` is `Some`, fetches and parses the JWKS. Any
    /// failure (network error, non-200 response, malformed JWKS, no usable
    /// key) returns `AuthError::Other` so the caller fails fast at startup.
    ///
    /// When `config.jwks_url` is `None`, logs a warning and returns a provider
    /// that rejects every token. Suitable for tests and for bring-up scenarios
    /// where the JWKS endpoint is not yet configured.
    pub fn new(config: OidcConfig) -> Result<Self, AuthError> {
        tracing::info!(
            issuer = %config.issuer,
            audience = %config.audience,
            jwks_url = ?config.jwks_url,
            "initializing OIDC auth provider"
        );
        let decoding_key = match config.jwks_url.as_deref() {
            Some(url) => Some(fetch_first_jwks_decoding_key(url)?),
            None => {
                warn!(
                    issuer = %config.issuer,
                    "OIDC provider has no JWKS URL configured — every token will be rejected. \
                     Set `jwks_url` in OidcConfig to enable validation."
                );
                None
            }
        };
        Ok(Self {
            config,
            decoding_key,
        })
    }

    /// Create with an explicit decoding key (for testing).
    pub fn with_key(config: OidcConfig, key: jsonwebtoken::DecodingKey) -> Self {
        Self {
            config,
            decoding_key: Some(key),
        }
    }

    /// Extract a claim value from a JWT payload by dot-path (e.g. "org.department").
    fn extract_claim(claims: &Value, path: &str) -> Option<String> {
        let parts: Vec<&str> = path.split('.').collect();
        let mut current = claims;
        for part in &parts {
            current = current.get(part)?;
        }
        current.as_str().map(str::to_string)
    }
}

#[async_trait]
impl AuthProvider for OidcAuthProvider {
    #[instrument(skip(self, credentials), fields(provider = "oidc"))]
    async fn authenticate(&self, credentials: &Credentials) -> Result<Identity, AuthError> {
        let result = self.do_authenticate(credentials);
        if let Err(ref err) = result {
            ServerMeters::get().auth_failures.add(
                1,
                &[
                    KeyValue::new("provider", "oidc"),
                    KeyValue::new("reason", auth_error_reason(err)),
                ],
            );
        }
        result
    }
}

impl OidcAuthProvider {
    fn do_authenticate(&self, credentials: &Credentials) -> Result<Identity, AuthError> {
        let token = match credentials {
            Credentials::Bearer(t) if t.is_empty() => {
                warn!(provider = "oidc", reason = "empty_token", "auth failure");
                return Err(AuthError::MissingCredentials);
            }
            Credentials::Bearer(t) => t,
            Credentials::ApiKey(_) => {
                warn!(
                    provider = "oidc",
                    reason = "wrong_credential_type",
                    "auth failure"
                );
                return Err(AuthError::Unauthorized);
            }
        };

        let decoding_key = self.decoding_key.as_ref().ok_or_else(|| {
            warn!(
                provider = "oidc",
                reason = "no_jwks",
                "OIDC provider has no JWKS — issuer may be unreachable at startup"
            );
            AuthError::Other("OIDC provider not initialized — issuer unreachable at startup".into())
        })?;

        let mut validation = jsonwebtoken::Validation::default();
        validation.leeway = 0;
        validation.set_audience(std::slice::from_ref(&self.config.audience));
        validation.set_issuer(std::slice::from_ref(&self.config.issuer));

        let token_data =
            jsonwebtoken::decode::<Value>(token, decoding_key, &validation).map_err(|e| {
                use jsonwebtoken::errors::ErrorKind;
                warn!(provider = "oidc", reason = %e, "auth failure");
                match e.kind() {
                    ErrorKind::ExpiredSignature => AuthError::Expired,
                    ErrorKind::InvalidSignature => AuthError::InvalidSignature,
                    _ => AuthError::Unauthorized,
                }
            })?;

        let claims = token_data.claims;
        let subject = claims["sub"]
            .as_str()
            .ok_or_else(|| {
                warn!(
                    provider = "oidc",
                    reason = "missing_sub_claim",
                    "auth failure"
                );
                AuthError::Unauthorized
            })?
            .to_string();
        let tenant_namespace = Self::extract_claim(&claims, &self.config.tenant_claim);
        let scopes = claims["scope"]
            .as_str()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();

        Ok(Identity {
            subject,
            tenant_namespace,
            scopes,
            metadata: claims,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// API key auth provider
// ──────────────────────────────────────────────────────────────────────────────

/// A single API key entry.
#[derive(Debug, Clone)]
pub struct ApiKeyEntry {
    pub key: String,
    pub subject: String,
    pub tenant_namespace: Option<String>,
    pub scopes: Vec<String>,
}

/// API key authentication provider.
///
/// Loads key definitions from the environment variable named in config
/// (`SIMULACRA_API_KEYS`). Format: `key:subject:tenant_namespace` (comma-separated entries).
pub struct ApiKeyAuthProvider {
    keys: HashMap<String, ApiKeyEntry>,
}

impl ApiKeyAuthProvider {
    /// Create an API key provider, loading keys from the given environment variable.
    pub fn from_env(env_var: &str) -> Self {
        let raw = std::env::var(env_var).unwrap_or_default();
        let mut keys = HashMap::new();
        for entry in raw.split(',').filter(|s| !s.is_empty()) {
            let parts: Vec<&str> = entry.trim().splitn(3, ':').collect();
            if parts.len() >= 2 {
                let key = parts[0].to_string();
                let subject = parts[1].to_string();
                let tenant_namespace = parts
                    .get(2)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                keys.insert(
                    key.clone(),
                    ApiKeyEntry {
                        key,
                        subject,
                        tenant_namespace,
                        scopes: vec!["tasks:create".into(), "tasks:manage".into()],
                    },
                );
            }
        }
        tracing::info!(
            key_count = keys.len(),
            env_var = %env_var,
            "initialized API key auth provider"
        );
        Self { keys }
    }

    /// Create an API key provider from an explicit list of entries (for testing).
    pub fn from_entries(entries: Vec<ApiKeyEntry>) -> Self {
        let keys = entries.into_iter().map(|e| (e.key.clone(), e)).collect();
        Self { keys }
    }
}

#[async_trait]
impl AuthProvider for ApiKeyAuthProvider {
    #[instrument(skip(self, credentials), fields(provider = "api_key"))]
    async fn authenticate(&self, credentials: &Credentials) -> Result<Identity, AuthError> {
        let result = self.do_authenticate(credentials);
        if let Err(ref err) = result {
            ServerMeters::get().auth_failures.add(
                1,
                &[
                    KeyValue::new("provider", "api_key"),
                    KeyValue::new("reason", auth_error_reason(err)),
                ],
            );
        }
        result
    }
}

impl ApiKeyAuthProvider {
    fn do_authenticate(&self, credentials: &Credentials) -> Result<Identity, AuthError> {
        let key = match credentials {
            Credentials::ApiKey(k) if k.is_empty() => {
                warn!(provider = "api_key", reason = "empty_key", "auth failure");
                return Err(AuthError::MissingCredentials);
            }
            Credentials::ApiKey(k) => k,
            Credentials::Bearer(_) => {
                warn!(
                    provider = "api_key",
                    reason = "wrong_credential_type",
                    "auth failure"
                );
                return Err(AuthError::Unauthorized);
            }
        };

        let entry = self.keys.get(key).ok_or_else(|| {
            warn!(provider = "api_key", reason = "unknown_key", "auth failure");
            AuthError::Unauthorized
        })?;

        Ok(Identity {
            subject: entry.subject.clone(),
            tenant_namespace: entry.tenant_namespace.clone(),
            scopes: entry.scopes.clone(),
            metadata: serde_json::json!({}),
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Composite auth provider
// ──────────────────────────────────────────────────────────────────────────────

/// Tries multiple auth providers in order. First to succeed wins.
pub struct CompositeAuthProvider {
    providers: Vec<Box<dyn AuthProvider>>,
}

impl CompositeAuthProvider {
    pub fn new(providers: Vec<Box<dyn AuthProvider>>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl AuthProvider for CompositeAuthProvider {
    async fn authenticate(&self, credentials: &Credentials) -> Result<Identity, AuthError> {
        let mut last_err = AuthError::Unauthorized;
        for provider in &self.providers {
            match provider.authenticate(credentials).await {
                Ok(identity) => return Ok(identity),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// NoAuth provider (dev-only)
// ──────────────────────────────────────────────────────────────────────────────

/// Auth provider that returns a fixed identity regardless of credentials.
///
/// **Dev-only.** Wire in via `[server.auth] dev_mode = true` in `simulacra.toml`
/// (or in the `examples/dev_server.rs` smoke entrypoint). Production deploys
/// must use `OidcAuthProvider`, `ApiKeyAuthProvider`, or `CompositeAuthProvider`.
pub struct NoAuthProvider {
    subject: String,
    tenant_namespace: String,
}

impl NoAuthProvider {
    pub fn new(subject: impl Into<String>, tenant_namespace: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            tenant_namespace: tenant_namespace.into(),
        }
    }
}

#[async_trait]
impl AuthProvider for NoAuthProvider {
    async fn authenticate(&self, _credentials: &Credentials) -> Result<Identity, AuthError> {
        Ok(Identity {
            subject: self.subject.clone(),
            tenant_namespace: Some(self.tenant_namespace.clone()),
            scopes: vec!["tasks:create".into(), "tasks:manage".into()],
            metadata: serde_json::json!({}),
        })
    }
}
