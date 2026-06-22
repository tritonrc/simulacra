//! S044 — `Tool` GraphQL type + `ToolKind` enum + tool queries.
//!
//! `Tool.id` IS the capability string the engine consumes via
//! `agent.capabilities[]`, so a UI picker can take a `Tool` and drop
//! `tool.id` straight into an `updateAgent { capabilities: [...] }` call.

use std::sync::Arc;

use async_graphql::{Context, Enum, ID, Object, SimpleObject};
use serde_json::Value;
use simulacra_catalog::CatalogError;
use simulacra_catalog::ids::AgentId;
use simulacra_catalog::repo::AgentRepository;

use crate::context::GraphQLContext;
use crate::error::to_field_error;
use crate::tool_catalog::ToolCatalog;

/// What kind of tool this is. Drives the picker grouping in the UI and
/// signals to the engine which capability-string namespace `Tool.id`
/// lives in.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ToolKind {
    /// `shell:exec`, `javascript`, `python` — boolean caps on the engine's
    /// per-task `CapabilityToken`.
    BuiltinCapability,
    /// `integration:<name>` — credentialed external service from the
    /// `simulacra-integration` registry.
    Integration,
    /// `mcp:<server>` — an MCP server registered with the engine.
    McpServer,
}

/// One selectable item in the agent-builder Tools picker.
///
/// `id` is the capability-string the engine consumes; it round-trips
/// 1:1 through `agent.capabilities[]`.
#[derive(SimpleObject, Clone, Debug)]
pub struct Tool {
    /// Capability string: `shell:exec` / `integration:slack` / `mcp:fetcher`.
    pub id: ID,
    pub kind: ToolKind,
    /// Human-readable label for the picker.
    pub name: String,
    /// Short description for the picker.
    pub description: String,
    /// `None` for built-in capabilities; integration name for `INTEGRATION`;
    /// server name for `MCP_SERVER`.
    pub provider: Option<String>,
    /// Reserved for sub-tool surfacing; always `None` in v1.
    pub input_schema: Option<Value>,
}

#[derive(Default)]
pub struct ToolQuery;

#[Object]
impl ToolQuery {
    /// All tools available to the authenticated tenant. Stable ordering:
    /// built-ins first (alpha by id), then integrations (alpha by id),
    /// then MCP servers (alpha by id). The catalog impl is free to return
    /// the list in any order — the resolver sorts before exposing.
    async fn available_tools(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Tool>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let catalog = ctx.data::<Arc<dyn ToolCatalog>>()?;
        let mut tools = catalog.list(&gql.tenant_id).await;
        tools.sort_by(|a, b| {
            (kind_order(a.kind), a.id.as_str()).cmp(&(kind_order(b.kind), b.id.as_str()))
        });
        Ok(tools)
    }

    /// Look up a single tool by its id (capability string). Returns `null`
    /// if the id is unknown to the tenant. NOT an error condition.
    async fn tool(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<Option<Tool>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let catalog = ctx.data::<Arc<dyn ToolCatalog>>()?;
        Ok(catalog.get(&gql.tenant_id, id.as_str()).await)
    }
}

fn kind_order(kind: ToolKind) -> u8 {
    match kind {
        ToolKind::BuiltinCapability => 0,
        ToolKind::Integration => 1,
        ToolKind::McpServer => 2,
    }
}

/// Project an agent's `capabilities[]` into structured `[Tool!]!`. Drops
/// capability strings that don't resolve in the catalog and emits a warn
/// log per dropped entry. Order matches `capabilities[]`; duplicates are
/// preserved (an agent that lists `shell:exec` twice gets `Tool` twice).
pub(crate) async fn project_agent_tools(
    ctx: &Context<'_>,
    agent_id: &AgentId,
) -> async_graphql::Result<Vec<Tool>> {
    let gql = ctx.data::<GraphQLContext>()?;
    let agents = ctx.data::<Arc<dyn AgentRepository>>()?;
    let catalog = ctx.data::<Arc<dyn ToolCatalog>>()?;

    let caps = match agents.capabilities(&gql.tenant_id, agent_id).await {
        Ok(c) => c,
        Err(CatalogError::NotFound(_)) => return Ok(Vec::new()),
        Err(e) => return Err(to_field_error(e)),
    };

    let mut out = Vec::with_capacity(caps.len());
    for cap in caps {
        match catalog.get(&gql.tenant_id, &cap).await {
            Some(tool) => out.push(tool),
            None => {
                tracing::warn!(
                    tenant_id = %gql.tenant_id.as_str(),
                    agent_id = %agent_id.as_str(),
                    unknown_capability = %cap,
                    "Agent.tools: dropping capability with no matching Tool in catalog",
                );
            }
        }
    }
    Ok(out)
}
