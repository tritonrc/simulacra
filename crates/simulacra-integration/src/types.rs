//! Integration runtime types — credentials, metadata, errors.
//!
//! Config types (`AuthMethod`, `IntegrationConfig`) live in `simulacra-config`.
//! This module has runtime-only types.

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

// Re-export config types so consumers can use simulacra_integration::{AuthMethod, IntegrationConfig}
pub use simulacra_config::{AuthMethod, IntegrationConfig};

/// Live credential state for a resolved integration.
pub struct IntegrationCredential {
    pub name: String,
    pub config: IntegrationConfig,
    /// Current access token (for OAuth2) or resolved API key value.
    pub access_token: RwLock<String>,
    /// When the current token expires (OAuth2 only).
    pub expires_at: RwLock<Option<Instant>>,
    /// Whether this integration is in degraded state.
    pub degraded: AtomicBool,
    /// Consecutive refresh failure count.
    pub refresh_failures: AtomicU32,
    /// Connectivity status: true = ok, false = failed.
    pub connectivity_ok: AtomicBool,
}

impl IntegrationCredential {
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    pub fn mark_degraded(&self) {
        self.degraded.store(true, Ordering::Relaxed);
    }

    pub fn clear_degraded(&self) {
        self.degraded.store(false, Ordering::Relaxed);
        self.refresh_failures.store(0, Ordering::Relaxed);
    }

    pub fn increment_failures(&self) -> u32 {
        self.refresh_failures.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Non-secret metadata exposed via VFS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationMetadata {
    pub base_url: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    pub rate_limit_rps: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Errors from the integration fabric.
#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("missing env var: {0}")]
    MissingEnvVar(String),

    #[error("integration not found: {0}")]
    NotFound(String),

    #[error("token refresh failed for integration: {0}")]
    TokenRefreshFailed(String),

    #[error("http error: {0}")]
    Http(String),

    #[error("integration degraded: {0}")]
    Degraded(String),
}
