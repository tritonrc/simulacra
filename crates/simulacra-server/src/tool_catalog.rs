//! Tool catalogs for the GraphQL tools surface.
//!
//! Why this exists (papercut #3): the GraphQL `Agent.tools` resolver and
//! `availableTools` query both consult an `Arc<dyn ToolCatalog>`. The
//! dev_server example previously wired in an `EmptyToolCatalog`, so every
//! capability on a seeded agent was dropped with a "no matching Tool in
//! catalog" warn and the agent-list UI rendered "0 tools".
//!
//! The runtime registers these boolean builtins keyed off
//! `agent.capabilities[]` (besides the always-on file/list builtins):
//!
//!   - `shell:exec` (alias `shell`)        → `shell_exec` tool
//!   - `javascript` (alias `js`)           → `js_exec` tool
//!   - `python`     (alias `py`)           → `py_exec` tool   *(feature = "python")*
//!
//! `python` is only listed when the `python` Cargo feature is compiled in —
//! gating the catalog on feature presence avoids the "phantom toolset" bug
//! (see `feedback_capability_strings.md`) where capabilities advertised by
//! the catalog have no matching tool at runtime.
//!
//! `BuiltinToolCatalog` remains available for tests and embedders that only
//! want the fixed runtime builtins. `DefaultToolCatalog` is the server-facing
//! catalog: it combines those builtins with config-time MCP servers so the UI
//! can grant server-level MCP access via `mcp:<server>`.

use std::collections::{HashMap, HashSet};

use async_graphql::ID;
use async_trait::async_trait;
use simulacra_catalog::ids::TenantId;
use simulacra_config::SimulacraConfig;
use simulacra_graphql::schema::{Tool, ToolKind};
use simulacra_graphql::tool_catalog::ToolCatalog;

/// Catalog of the built-in capabilities the runtime registers as real tools.
///
/// Tenant-agnostic: every tenant sees the same builtins (the runtime itself
/// does not currently scope builtins per tenant). Instantiate with the bare
/// `BuiltinToolCatalog` (it is a unit struct with no state).
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinToolCatalog;

impl BuiltinToolCatalog {
    /// The fixed builtin set, freshly cloned. Order is unspecified here;
    /// `list()` sorts before returning so the result is deterministic.
    fn builtins() -> Vec<Tool> {
        vec![
            Tool {
                id: ID::from("shell:exec"),
                kind: ToolKind::BuiltinCapability,
                name: "shell".to_string(),
                description: "Run a virtual POSIX shell command (echo, cat, \
                    ls -l/-a/-la, mkdir, grep, cd, pwd, env, which; operators \
                    &&, ||, ;, |). Cwd persists across calls."
                    .to_string(),
                provider: None,
                input_schema: None,
            },
            Tool {
                id: ID::from("javascript"),
                kind: ToolKind::BuiltinCapability,
                name: "javascript".to_string(),
                description: "Execute JavaScript in a single-shot QuickJS \
                    context via js_exec. Globals, prototypes, and module \
                    singletons do not persist between invocations."
                    .to_string(),
                provider: None,
                input_schema: None,
            },
            #[cfg(feature = "python")]
            Tool {
                id: ID::from("python"),
                kind: ToolKind::BuiltinCapability,
                name: "python".to_string(),
                description: "Execute Python code in the Monty runtime via \
                    py_exec. Single-shot per call (no persistent globals \
                    between invocations); has access to file/list/http \
                    bridges through the agent cell."
                    .to_string(),
                provider: None,
                input_schema: None,
            },
        ]
    }
}

#[async_trait]
impl ToolCatalog for BuiltinToolCatalog {
    async fn list(&self, _tenant_id: &TenantId) -> Vec<Tool> {
        let mut tools = Self::builtins();
        tools.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        tools
    }

    async fn get(&self, _tenant_id: &TenantId, id: &str) -> Option<Tool> {
        Self::builtins().into_iter().find(|t| t.id.as_str() == id)
    }
}

/// Server-facing tool catalog backed by static startup configuration.
///
/// MCP server rows intentionally come from `[mcp.servers]` only. Runtime MCP
/// tool discovery is per-task and may fail or vary by transport; the UI picker
/// grants access to the configured server, not to individual discovered tools.
#[derive(Debug, Clone, Default)]
pub struct DefaultToolCatalog {
    global_mcp_servers: Vec<String>,
    tenant_mcp_servers: HashMap<TenantId, Vec<String>>,
}

impl DefaultToolCatalog {
    pub fn from_config(config: &SimulacraConfig) -> Self {
        let mcp_servers = configured_mcp_server_names(config);
        let global_mcp_servers = if config.tenants.is_empty() {
            mcp_servers
        } else {
            Vec::new()
        };
        Self {
            global_mcp_servers,
            tenant_mcp_servers: HashMap::new(),
        }
    }

    pub fn from_config_for_tenants<I>(config: &SimulacraConfig, tenants: I) -> Self
    where
        I: IntoIterator<Item = (TenantId, String)>,
    {
        let configured: HashSet<String> = configured_mcp_server_names(config).into_iter().collect();
        let mut tenant_mcp_servers = HashMap::new();

        for (tenant_id, namespace) in tenants {
            let mut servers = config
                .tenants
                .get(&namespace)
                .and_then(|tenant| tenant.mcp_servers.as_ref())
                .map(|names| {
                    names
                        .iter()
                        .filter(|name| configured.contains(*name))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            servers.sort();
            servers.dedup();
            tenant_mcp_servers.insert(tenant_id, servers);
        }

        Self {
            global_mcp_servers: Vec::new(),
            tenant_mcp_servers,
        }
    }

    fn mcp_servers_for(&self, tenant_id: &TenantId) -> &[String] {
        self.tenant_mcp_servers
            .get(tenant_id)
            .map(Vec::as_slice)
            .unwrap_or(self.global_mcp_servers.as_slice())
    }

    fn mcp_tool(server: &str) -> Tool {
        Tool {
            id: ID::from(format!("mcp:{server}")),
            kind: ToolKind::McpServer,
            name: format!("{server} MCP"),
            description: format!("Grant access to the configured MCP server '{server}'."),
            provider: Some(server.to_string()),
            input_schema: None,
        }
    }

    fn mcp_tools_for(&self, tenant_id: &TenantId) -> Vec<Tool> {
        self.mcp_servers_for(tenant_id)
            .iter()
            .map(|server| Self::mcp_tool(server))
            .collect()
    }
}

fn configured_mcp_server_names(config: &SimulacraConfig) -> Vec<String> {
    let mut mcp_servers: Vec<String> = config
        .mcp
        .as_ref()
        .map(|mcp| {
            mcp.servers
                .iter()
                .map(|server| server.name.clone())
                .collect()
        })
        .unwrap_or_default();
    mcp_servers.sort();
    mcp_servers.dedup();
    mcp_servers
}

#[async_trait]
impl ToolCatalog for DefaultToolCatalog {
    async fn list(&self, tenant_id: &TenantId) -> Vec<Tool> {
        let mut tools = BuiltinToolCatalog.list(tenant_id).await;
        tools.extend(self.mcp_tools_for(tenant_id));
        tools
    }

    async fn get(&self, tenant_id: &TenantId, id: &str) -> Option<Tool> {
        if let Some(tool) = BuiltinToolCatalog.get(tenant_id, id).await {
            return Some(tool);
        }
        let server = id.strip_prefix("mcp:")?;
        if server.is_empty() || server.contains(':') {
            return None;
        }
        self.mcp_servers_for(tenant_id)
            .iter()
            .any(|name| name == server)
            .then(|| Self::mcp_tool(server))
    }
}
