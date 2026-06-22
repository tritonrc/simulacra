//! S044 — Tool catalog trait.
//!
//! The GraphQL surface enumerates "tools" (built-in capabilities,
//! integrations, MCP servers) without owning the underlying registries.
//! Concrete implementations (e.g. `simulacra-server::tool_catalog::DefaultToolCatalog`)
//! combine `IntegrationRegistry`, MCP config, and the fixed builtins into a
//! single tenant-scoped view.
//!
//! Read-only by design: mutations stay on `updateAgent { capabilities }`
//! per spec §Scope.

use async_trait::async_trait;
use simulacra_catalog::ids::TenantId;

use crate::schema::Tool;

/// Tenant-scoped enumerator of tools available in the agent-builder UI.
///
/// Every method takes `&TenantId` first to mirror the `simulacra-catalog`
/// repository convention and prevent cross-tenant leakage at the type
/// boundary. Implementations MUST filter by tenant before returning.
#[async_trait]
pub trait ToolCatalog: Send + Sync {
    /// All tools available to `tenant_id`. Order is implementation-defined;
    /// the GraphQL resolver sorts the result before exposing it.
    async fn list(&self, tenant_id: &TenantId) -> Vec<Tool>;

    /// Lookup a single tool by its `id` (capability string).
    /// Returns `None` if `id` is unknown OR belongs to a different tenant.
    async fn get(&self, tenant_id: &TenantId, id: &str) -> Option<Tool>;
}
