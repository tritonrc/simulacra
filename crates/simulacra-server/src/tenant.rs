//! Tenant resolution: maps authenticated identities to tenant configurations.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::auth::Identity;

/// Budget pool configuration for a tenant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetPoolConfig {
    /// Maximum token spend (0 = unlimited).
    #[serde(default)]
    pub max_tokens: u64,
    /// Maximum cost as a decimal string (e.g. "500.00"). Empty = unlimited.
    #[serde(default)]
    pub max_cost: String,
}

/// Configuration for a single tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantConfig {
    /// Unique namespace identifier for this tenant.
    pub namespace: String,
    /// Default agent type for tasks created without an explicit agent_type.
    pub agent_type: String,
    /// VFS root directory for this tenant's agents.
    pub vfs_root: PathBuf,
    /// Budget pool governing all tasks under this tenant.
    pub budget_pool: BudgetPoolConfig,
    /// Governance hook names applied to tasks under this tenant.
    pub hooks: Vec<String>,
    /// Integration names this tenant is allowed to use.
    /// Empty = no integrations (in multi-tenant mode).
    /// Single-tenant mode falls back to all integrations.
    #[serde(default)]
    pub integrations: Vec<String>,
    /// MCP server names this tenant is allowed to use.
    /// Empty = no MCP servers in multi-tenant mode.
    /// Single-tenant mode falls back to all configured MCP servers.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
}

impl TenantConfig {
    /// Validate that this tenant config is complete and usable at startup.
    pub fn validate(&self) -> Result<(), String> {
        if self.agent_type.is_empty() {
            return Err(format!(
                "tenant '{}': agent_type is required",
                self.namespace
            ));
        }
        if self.vfs_root.as_os_str().is_empty() {
            return Err(format!("tenant '{}': vfs_root is required", self.namespace));
        }
        Ok(())
    }
}

/// Errors returned by `TenantResolver::resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TenantError {
    /// No tenant could be found for the given hint.
    #[error("tenant not found")]
    NotFound,

    /// No tenant hint and no default tenant configured, or hint points to unknown tenant.
    #[error("tenant forbidden — no tenant resolved for identity")]
    Forbidden,
}

/// Maps an authenticated `Identity` to a `TenantConfig`.
///
/// Resolution order:
/// 1. `identity.tenant_namespace` → direct lookup
/// 2. `default_tenant` → fallback
/// 3. Neither → `TenantError::Forbidden`
#[derive(Debug, Clone, Default)]
pub struct TenantResolver {
    tenants: HashMap<String, TenantConfig>,
    default_tenant: Option<String>,
}

impl TenantResolver {
    pub fn new(tenants: HashMap<String, TenantConfig>, default_tenant: Option<String>) -> Self {
        Self {
            tenants,
            default_tenant,
        }
    }

    /// Resolve an identity to a tenant config.
    pub fn resolve(&self, identity: &Identity) -> Result<&TenantConfig, TenantError> {
        // 1. Try the tenant_namespace from the identity first.
        if let Some(ns) = &identity.tenant_namespace {
            return self.tenants.get(ns).ok_or_else(|| {
                warn!(
                    subject = %identity.subject,
                    tenant_namespace = %ns,
                    "tenant resolution failure: namespace does not match any configured tenant"
                );
                TenantError::Forbidden
            });
        }

        // 2. Fall back to the configured default tenant.
        if let Some(default_name) = &self.default_tenant {
            return self.tenants.get(default_name).ok_or(TenantError::NotFound);
        }

        // 3. No resolution possible.
        warn!(
            subject = %identity.subject,
            "tenant resolution failure: no tenant_namespace and no default tenant configured"
        );
        Err(TenantError::Forbidden)
    }

    /// Retrieve a tenant config directly by namespace (for internal trigger use).
    pub fn get(&self, namespace: &str) -> Option<&TenantConfig> {
        self.tenants.get(namespace)
    }

    /// Number of configured tenants.
    pub fn tenant_count(&self) -> usize {
        self.tenants.len()
    }

    /// Validate all tenant configs at startup.
    pub fn validate_all(&self) -> Vec<String> {
        self.tenants
            .values()
            .filter_map(|t| t.validate().err())
            .collect()
    }
}
