//! Papercut #3 — `BuiltinToolCatalog` returns the fixed runtime builtins
//! (`shell:exec`, `javascript`, and `python` when the `python` Cargo feature
//! is on) so the GraphQL `Agent.tools` resolver and `availableTools` query
//! show real tools in dev_server instead of dropping every capability with a
//! "no matching Tool in catalog" warn.
//!
//! Asserted invariants:
//!   - `list()` returns the builtins (2 base + python when feature is on),
//!     sorted by id, for any tenant.
//!   - `get("shell:exec")` returns Some with kind BuiltinCapability.
//!   - `get("javascript")` returns Some with kind BuiltinCapability.
//!   - `get("python")` returns Some when feature compiled in, None otherwise
//!     — guards the phantom-toolset bug when python is opted out at build.
//!   - `get("nonsense")` returns None.
//!   - Tenant-agnostic: a different tenant id sees the same set.

use simulacra_catalog::ids::TenantId;
use simulacra_config::{McpConfig, McpServerConfig, ProjectConfig, SimulacraConfig, VfsConfig};
use simulacra_graphql::schema::ToolKind;
use simulacra_graphql::tool_catalog::ToolCatalog;
use simulacra_server::{BuiltinToolCatalog, DefaultToolCatalog};

fn config_with_mcp_servers(names: &[&str]) -> SimulacraConfig {
    SimulacraConfig {
        project: ProjectConfig {
            name: "tool-catalog-test".to_string(),
            description: None,
        },
        agent_types: Default::default(),
        integrations: Default::default(),
        tenants: Default::default(),
        mcp: Some(McpConfig {
            servers: names
                .iter()
                .map(|name| McpServerConfig {
                    name: (*name).to_string(),
                    transport: Some("http".to_string()),
                    url: Some(format!("http://{name}.local/mcp")),
                    module: None,
                    env: None,
                    network: vec![],
                    wasi: None,
                })
                .collect(),
        }),
        task: None,
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: Default::default(),
    }
}

fn tenant_scoped_config() -> SimulacraConfig {
    let mut config = config_with_mcp_servers(&["fetcher", "planner"]);
    config.tenants.insert(
        "alpha".to_string(),
        simulacra_config::TenantConfig {
            agent_type: "agent".to_string(),
            integrations: None,
            mcp_servers: Some(vec!["fetcher".to_string()]),
        },
    );
    config.tenants.insert(
        "beta".to_string(),
        simulacra_config::TenantConfig {
            agent_type: "agent".to_string(),
            integrations: None,
            mcp_servers: Some(vec!["planner".to_string()]),
        },
    );
    config
}

fn tenant() -> TenantId {
    TenantId::from("default")
}

#[tokio::test]
async fn list_returns_builtins_sorted_by_id() {
    let catalog = BuiltinToolCatalog;
    let tools = catalog.list(&tenant()).await;

    let ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();

    // The base builtins are always present in id-sorted order.
    // `python` shows up between `javascript` and `shell:exec` only when the
    // `python` Cargo feature is compiled in.
    #[cfg(feature = "python")]
    {
        assert_eq!(ids, vec!["javascript", "python", "shell:exec"]);
    }
    #[cfg(not(feature = "python"))]
    {
        assert_eq!(ids, vec!["javascript", "shell:exec"]);
    }

    for tool in &tools {
        assert_eq!(tool.kind, ToolKind::BuiltinCapability);
        assert!(tool.provider.is_none(), "builtins have no provider");
        assert!(
            tool.input_schema.is_none(),
            "input_schema reserved (None in v1)"
        );
        assert!(!tool.name.is_empty(), "name must be set for picker");
        assert!(
            !tool.description.is_empty(),
            "description must be set for picker"
        );
    }
}

#[tokio::test]
async fn list_is_tenant_agnostic() {
    let catalog = BuiltinToolCatalog;
    let a = catalog.list(&TenantId::from("alpha")).await;
    let b = catalog.list(&TenantId::from("beta")).await;

    let a_ids: Vec<_> = a.iter().map(|t| t.id.as_str().to_string()).collect();
    let b_ids: Vec<_> = b.iter().map(|t| t.id.as_str().to_string()).collect();

    assert_eq!(a_ids, b_ids, "builtin set must not vary by tenant");
}

#[tokio::test]
async fn get_shell_exec_returns_builtin_capability() {
    let catalog = BuiltinToolCatalog;
    let tool = catalog
        .get(&tenant(), "shell:exec")
        .await
        .expect("shell:exec must resolve");

    assert_eq!(tool.id.as_str(), "shell:exec");
    assert_eq!(tool.kind, ToolKind::BuiltinCapability);
    assert_eq!(tool.name, "shell");
    assert!(tool.provider.is_none());
    assert!(tool.input_schema.is_none());
    assert!(
        tool.description.to_lowercase().contains("shell"),
        "description should reference shell, got: {}",
        tool.description
    );
}

#[tokio::test]
async fn get_javascript_returns_builtin_capability() {
    let catalog = BuiltinToolCatalog;
    let tool = catalog
        .get(&tenant(), "javascript")
        .await
        .expect("javascript must resolve");

    assert_eq!(tool.id.as_str(), "javascript");
    assert_eq!(tool.kind, ToolKind::BuiltinCapability);
    assert_eq!(tool.name, "javascript");
    assert!(tool.provider.is_none());
    assert!(tool.input_schema.is_none());
    assert!(
        tool.description.to_lowercase().contains("javascript")
            || tool.description.to_lowercase().contains("quickjs"),
        "description should reference JavaScript/QuickJS, got: {}",
        tool.description
    );
}

#[cfg(feature = "python")]
#[tokio::test]
async fn get_python_returns_builtin_capability_when_feature_enabled() {
    let catalog = BuiltinToolCatalog;
    let tool = catalog
        .get(&tenant(), "python")
        .await
        .expect("python must resolve when python feature is on");

    assert_eq!(tool.id.as_str(), "python");
    assert_eq!(tool.kind, ToolKind::BuiltinCapability);
    assert_eq!(tool.name, "python");
    assert!(tool.description.to_lowercase().contains("python"));
}

#[cfg(feature = "python")]
#[tokio::test]
async fn get_py_alias_is_not_in_catalog() {
    // The catalog uses canonical capability ids only. `py` is a parser alias,
    // not a separate catalog entry — agents seed `python`, the parser handles
    // both. Adding `py` here would double-count in the picker.
    let catalog = BuiltinToolCatalog;
    assert!(catalog.get(&tenant(), "py").await.is_none());
}

#[cfg(not(feature = "python"))]
#[tokio::test]
async fn get_python_returns_none_without_feature() {
    // When the `python` Cargo feature is opted out at build, py_exec is not
    // registered. Surfacing `python` here would re-create the phantom-toolset
    // bug for embed contexts that drop the feature.
    let catalog = BuiltinToolCatalog;
    assert!(catalog.get(&tenant(), "python").await.is_none());
    assert!(catalog.get(&tenant(), "py").await.is_none());
}

#[tokio::test]
async fn get_unknown_id_returns_none() {
    let catalog = BuiltinToolCatalog;
    assert!(catalog.get(&tenant(), "nonsense").await.is_none());
    assert!(catalog.get(&tenant(), "").await.is_none());
    assert!(catalog.get(&tenant(), "integration:slack").await.is_none());
    assert!(catalog.get(&tenant(), "mcp:fetcher").await.is_none());
}

#[tokio::test]
async fn get_is_tenant_agnostic_for_builtins() {
    let catalog = BuiltinToolCatalog;
    let alpha = catalog
        .get(&TenantId::from("alpha"), "shell:exec")
        .await
        .expect("alpha tenant gets shell:exec");
    let beta = catalog
        .get(&TenantId::from("beta"), "shell:exec")
        .await
        .expect("beta tenant gets shell:exec");

    assert_eq!(alpha.id.as_str(), beta.id.as_str());
    assert_eq!(alpha.name, beta.name);
    assert_eq!(alpha.description, beta.description);
}

#[tokio::test]
async fn default_catalog_lists_configured_mcp_servers_after_builtins() {
    let config = config_with_mcp_servers(&["zfetch", "abridge"]);
    let catalog = DefaultToolCatalog::from_config(&config);

    let tools = catalog.list(&tenant()).await;
    let mcp: Vec<_> = tools
        .iter()
        .filter(|tool| tool.kind == ToolKind::McpServer)
        .map(|tool| {
            (
                tool.id.as_str().to_string(),
                tool.name.clone(),
                tool.provider.clone(),
            )
        })
        .collect();

    assert_eq!(
        mcp,
        vec![
            (
                "mcp:abridge".to_string(),
                "abridge MCP".to_string(),
                Some("abridge".to_string())
            ),
            (
                "mcp:zfetch".to_string(),
                "zfetch MCP".to_string(),
                Some("zfetch".to_string())
            ),
        ],
        "configured MCP servers should be visible as server-level picker tools"
    );
}

#[tokio::test]
async fn default_catalog_get_resolves_configured_mcp_server() {
    let config = config_with_mcp_servers(&["fetcher"]);
    let catalog = DefaultToolCatalog::from_config(&config);

    let tool = catalog
        .get(&tenant(), "mcp:fetcher")
        .await
        .expect("configured MCP server should resolve");

    assert_eq!(tool.id.as_str(), "mcp:fetcher");
    assert_eq!(tool.kind, ToolKind::McpServer);
    assert_eq!(tool.provider.as_deref(), Some("fetcher"));
    assert!(tool.input_schema.is_none());
    assert!(
        tool.description.contains("configured MCP server"),
        "description should make clear this is a configured MCP server"
    );
}

#[tokio::test]
async fn default_catalog_does_not_resolve_unconfigured_mcp_server() {
    let config = config_with_mcp_servers(&["fetcher"]);
    let catalog = DefaultToolCatalog::from_config(&config);

    assert!(catalog.get(&tenant(), "mcp:planner").await.is_none());
    assert!(catalog.get(&tenant(), "mcp:fetcher:*").await.is_none());
}

#[tokio::test]
async fn default_catalog_scopes_mcp_servers_by_tenant_when_tenant_map_is_supplied() {
    let config = tenant_scoped_config();
    let alpha = TenantId::from("alpha-id");
    let beta = TenantId::from("beta-id");
    let catalog = DefaultToolCatalog::from_config_for_tenants(
        &config,
        [
            (alpha.clone(), "alpha".to_string()),
            (beta.clone(), "beta".to_string()),
        ],
    );

    let alpha_tools = catalog.list(&alpha).await;
    let beta_tools = catalog.list(&beta).await;

    assert!(
        alpha_tools
            .iter()
            .any(|tool| tool.id.as_str() == "mcp:fetcher"),
        "alpha should see its configured MCP server"
    );
    assert!(
        !alpha_tools
            .iter()
            .any(|tool| tool.id.as_str() == "mcp:planner"),
        "alpha must not see beta's MCP server"
    );
    assert!(
        beta_tools
            .iter()
            .any(|tool| tool.id.as_str() == "mcp:planner"),
        "beta should see its configured MCP server"
    );
    assert!(
        !beta_tools
            .iter()
            .any(|tool| tool.id.as_str() == "mcp:fetcher"),
        "beta must not see alpha's MCP server"
    );
    assert!(catalog.get(&alpha, "mcp:fetcher").await.is_some());
    assert!(catalog.get(&alpha, "mcp:planner").await.is_none());
}
