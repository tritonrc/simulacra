use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::{EmptySubscription, Schema};
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Catalog, Tenant};
use simulacra_config::{McpConfig, McpServerConfig, ProjectConfig, SimulacraConfig, VfsConfig};
use simulacra_graphql::context::{AuthenticatedPrincipal, GraphQLContext};
use simulacra_graphql::schema::{MutationRoot, QueryRoot};
use simulacra_graphql::tool_catalog::ToolCatalog;
use simulacra_server::DefaultToolCatalog;

fn config_with_mcp_servers(names: &[&str]) -> SimulacraConfig {
    let mut config = SimulacraConfig {
        project: ProjectConfig {
            name: "tool-catalog-graphql".to_string(),
            description: None,
        },
        agent_types: HashMap::new(),
        integrations: HashMap::new(),
        tenants: HashMap::new(),
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
    };
    config.tenants.insert(
        "default".to_string(),
        simulacra_config::TenantConfig {
            agent_type: "agent".to_string(),
            integrations: None,
            mcp_servers: Some(names.iter().map(|name| (*name).to_string()).collect()),
        },
    );
    config
}

async fn tenant(catalog: &Catalog) -> Tenant {
    catalog
        .tenants()
        .create("default", Some("default"))
        .await
        .expect("tenant should be created")
}

#[tokio::test]
async fn available_tools_query_exposes_configured_mcp_servers() {
    let catalog = Catalog::open_in_memory().expect("catalog");
    let tenant = tenant(&catalog).await;
    let config = config_with_mcp_servers(&["fetcher"]);
    let tool_catalog = DefaultToolCatalog::from_config_for_tenants(
        &config,
        [(tenant.id.clone(), "default".to_string())],
    );

    let schema = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::new(catalog.agents()) as Arc<dyn AgentRepository>)
    .data(Arc::new(catalog.skills()) as Arc<dyn SkillRepository>)
    .data(Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>)
    .data(Arc::new(tool_catalog) as Arc<dyn ToolCatalog>)
    .data(GraphQLContext {
        tenant_id: tenant.id,
        principal: AuthenticatedPrincipal {
            tenant_namespace: "default".to_string(),
            subject: "test-user".to_string(),
        },
    })
    .finish();

    let response = schema
        .execute("{ availableTools { id kind name provider } }")
        .await;
    assert!(
        response.errors.is_empty(),
        "availableTools query should succeed: {:?}",
        response.errors
    );
    let data = response.data.into_json().expect("data should be JSON");
    let tools = data["availableTools"]
        .as_array()
        .expect("availableTools should be an array");

    assert!(
        tools.iter().any(|tool| {
            tool["id"] == "mcp:fetcher"
                && tool["kind"] == "MCP_SERVER"
                && tool["name"] == "fetcher MCP"
                && tool["provider"] == "fetcher"
        }),
        "availableTools should include the configured MCP server; got {tools:?}"
    );
}
