use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::{EmptySubscription, Schema};
use async_trait::async_trait;
use serde_json::{Value, json};
use simulacra_catalog::ids::TenantId;
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Catalog, NewAgent};
use simulacra_graphql::context::{AuthenticatedPrincipal, GraphQLContext};
use simulacra_graphql::schema::{MutationRoot, QueryRoot, SimulacraSchema, Tool, ToolKind};
use simulacra_graphql::tool_catalog::ToolCatalog;

#[derive(Clone, Debug, Default)]
struct StubToolCatalog {
    tools_by_tenant: HashMap<TenantId, Vec<Tool>>,
}

fn stub_tool_catalog(entries: impl IntoIterator<Item = (TenantId, Vec<Tool>)>) -> StubToolCatalog {
    StubToolCatalog {
        tools_by_tenant: entries.into_iter().collect(),
    }
}

#[async_trait]
impl ToolCatalog for StubToolCatalog {
    async fn list(&self, tenant_id: &TenantId) -> Vec<Tool> {
        self.tools_by_tenant
            .get(tenant_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn get(&self, tenant_id: &TenantId, id: &str) -> Option<Tool> {
        self.tools_by_tenant
            .get(tenant_id)
            .and_then(|tools| tools.iter().find(|tool| tool.id.as_str() == id))
            .cloned()
    }
}

#[derive(Clone, Debug)]
struct AgentSeed {
    name: &'static str,
    capabilities: Vec<String>,
}

struct TenantFixture {
    schema: SimulacraSchema,
    #[allow(dead_code)]
    tenant: simulacra_catalog::Tenant,
    agents: Vec<simulacra_catalog::Agent>,
}

#[allow(dead_code)] // Several fields are read by some tests, not others.
struct TwoTenantFixture {
    schema_a: SimulacraSchema,
    schema_b: SimulacraSchema,
    tenant_a: simulacra_catalog::Tenant,
    tenant_b: simulacra_catalog::Tenant,
    tenant_a_agents: Vec<simulacra_catalog::Agent>,
    tenant_b_agents: Vec<simulacra_catalog::Agent>,
}

fn builtin_tool(id: &str, name: &str, description: &str) -> Tool {
    Tool {
        id: id.into(),
        kind: ToolKind::BuiltinCapability,
        name: name.to_owned(),
        description: description.to_owned(),
        provider: None,
        input_schema: None,
    }
}

fn integration_tool(name: &str, description: &str) -> Tool {
    Tool {
        id: format!("integration:{name}").into(),
        kind: ToolKind::Integration,
        name: title_case(name),
        description: description.to_owned(),
        provider: Some(name.to_owned()),
        input_schema: None,
    }
}

fn mcp_tool(server_name: &str, description: &str) -> Tool {
    Tool {
        id: format!("mcp:{server_name}").into(),
        kind: ToolKind::McpServer,
        name: format!("{server_name} MCP"),
        description: description.to_owned(),
        provider: Some(server_name.to_owned()),
        input_schema: None,
    }
}

fn builtin_tools() -> Vec<Tool> {
    vec![
        builtin_tool("shell:exec", "Shell execution", "Execute shell commands"),
        builtin_tool("javascript", "JavaScript", "Run JavaScript snippets"),
        builtin_tool("python", "Python", "Run Python scripts"),
    ]
}

fn title_case(input: &str) -> String {
    let mut chars = input.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn build_schema(
    catalog: &Catalog,
    tenant: &simulacra_catalog::Tenant,
    tool_catalog: Arc<dyn ToolCatalog>,
) -> SimulacraSchema {
    Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::new(catalog.agents()) as Arc<dyn AgentRepository>)
    .data(Arc::new(catalog.skills()) as Arc<dyn SkillRepository>)
    .data(Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>)
    .data(tool_catalog)
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant.namespace.clone(),
            subject: "test-user".to_owned(),
        },
    })
    .finish()
}

async fn schema_with_seeded_catalog(
    tenant_namespace: &str,
    tools: Vec<Tool>,
    agents: Vec<AgentSeed>,
) -> TenantFixture {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create(tenant_namespace, Some(tenant_namespace))
        .await
        .unwrap();

    let mut seeded_agents = Vec::new();
    for agent in agents {
        seeded_agents.push(
            catalog
                .agents()
                .create(
                    &tenant.id,
                    NewAgent {
                        name: agent.name,
                        description: Some("test agent"),
                        system_prompt: "system prompt",
                        model: "gpt-test",
                        max_turns: Some(8),
                        max_tokens: Some(2048),
                        memory_pool_id: None,
                        skill_ids: &[],
                        capabilities: &agent.capabilities,
                        channel_ids: &[],
                    },
                )
                .await
                .unwrap(),
        );
    }

    let schema = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::new(catalog.agents()) as Arc<dyn AgentRepository>)
    .data(Arc::new(catalog.skills()) as Arc<dyn SkillRepository>)
    .data(Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>)
    .data(Arc::new(stub_tool_catalog([(tenant.id.clone(), tools)])) as Arc<dyn ToolCatalog>)
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant.namespace.clone(),
            subject: "test-user".to_owned(),
        },
    })
    .finish();

    TenantFixture {
        schema,
        tenant,
        agents: seeded_agents,
    }
}

async fn schema_with_two_tenants(
    tenant_a_tools: Vec<Tool>,
    tenant_b_tools: Vec<Tool>,
    tenant_a_agents: Vec<AgentSeed>,
    tenant_b_agents: Vec<AgentSeed>,
) -> TwoTenantFixture {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant_a = catalog
        .tenants()
        .create("tenant-a", Some("Tenant A"))
        .await
        .unwrap();
    let tenant_b = catalog
        .tenants()
        .create("tenant-b", Some("Tenant B"))
        .await
        .unwrap();

    let mut seeded_tenant_a_agents = Vec::new();
    for agent in tenant_a_agents {
        seeded_tenant_a_agents.push(
            catalog
                .agents()
                .create(
                    &tenant_a.id,
                    NewAgent {
                        name: agent.name,
                        description: Some("tenant a agent"),
                        system_prompt: "system prompt",
                        model: "gpt-test",
                        max_turns: Some(8),
                        max_tokens: Some(2048),
                        memory_pool_id: None,
                        skill_ids: &[],
                        capabilities: &agent.capabilities,
                        channel_ids: &[],
                    },
                )
                .await
                .unwrap(),
        );
    }

    let mut seeded_tenant_b_agents = Vec::new();
    for agent in tenant_b_agents {
        seeded_tenant_b_agents.push(
            catalog
                .agents()
                .create(
                    &tenant_b.id,
                    NewAgent {
                        name: agent.name,
                        description: Some("tenant b agent"),
                        system_prompt: "system prompt",
                        model: "gpt-test",
                        max_turns: Some(8),
                        max_tokens: Some(2048),
                        memory_pool_id: None,
                        skill_ids: &[],
                        capabilities: &agent.capabilities,
                        channel_ids: &[],
                    },
                )
                .await
                .unwrap(),
        );
    }

    let tool_catalog = Arc::new(stub_tool_catalog([
        (tenant_a.id.clone(), tenant_a_tools),
        (tenant_b.id.clone(), tenant_b_tools),
    ])) as Arc<dyn ToolCatalog>;

    TwoTenantFixture {
        schema_a: build_schema(&catalog, &tenant_a, Arc::clone(&tool_catalog)),
        schema_b: build_schema(&catalog, &tenant_b, tool_catalog),
        tenant_a,
        tenant_b,
        tenant_a_agents: seeded_tenant_a_agents,
        tenant_b_agents: seeded_tenant_b_agents,
    }
}

async fn execute_json(schema: &SimulacraSchema, query: impl Into<String>) -> Value {
    let response = schema.execute(query.into()).await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    response.data.into_json().unwrap()
}

fn tool_ids(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn tool_id_round_trips_through_agent_capabilities_for_all_three_kinds() {
    let tools = vec![
        integration_tool("slack", "Post messages to Slack"),
        builtin_tool("shell:exec", "Shell execution", "Execute shell commands"),
        mcp_tool("fetcher", "Fetch remote content"),
    ];
    let fixture = schema_with_seeded_catalog(
        "acme",
        tools,
        vec![AgentSeed {
            name: "round-trip-agent",
            capabilities: vec![
                "shell:exec".to_owned(),
                "integration:slack".to_owned(),
                "mcp:fetcher".to_owned(),
            ],
        }],
    )
    .await;

    let data = execute_json(
        &fixture.schema,
        format!(
            r#"{{
                agent(id: "{}") {{
                    capabilities
                    tools {{ id }}
                }}
            }}"#,
            fixture.agents[0].id.as_str()
        ),
    )
    .await;

    // The agent_capabilities table is `PRIMARY KEY (agent_id, capability)`
    // and the repo returns rows ORDER BY capability ASC, so both
    // `capabilities` and `tools` come back alphabetically.
    assert_eq!(
        data["agent"]["capabilities"],
        json!(["integration:slack", "mcp:fetcher", "shell:exec"])
    );
    assert_eq!(
        data["agent"]["tools"],
        json!([
            { "id": "integration:slack" },
            { "id": "mcp:fetcher" },
            { "id": "shell:exec" }
        ])
    );
}

#[tokio::test]
async fn tool_kind_is_builtin_capability_for_shell_javascript_python() {
    let fixture = schema_with_seeded_catalog("acme", builtin_tools(), vec![]).await;

    let data = execute_json(
        &fixture.schema,
        r#"{
            shell: tool(id: "shell:exec") { kind }
            javascript: tool(id: "javascript") { kind }
            python: tool(id: "python") { kind }
        }"#,
    )
    .await;

    assert_eq!(data["shell"]["kind"], "BUILTIN_CAPABILITY");
    assert_eq!(data["javascript"]["kind"], "BUILTIN_CAPABILITY");
    assert_eq!(data["python"]["kind"], "BUILTIN_CAPABILITY");
}

#[tokio::test]
async fn tool_kind_is_integration_for_integration_prefix() {
    let fixture = schema_with_seeded_catalog(
        "acme",
        vec![integration_tool("slack", "Post messages to Slack")],
        vec![],
    )
    .await;

    let data = execute_json(
        &fixture.schema,
        r#"{
            tool(id: "integration:slack") { id kind }
        }"#,
    )
    .await;

    assert_eq!(data["tool"]["id"], "integration:slack");
    assert_eq!(data["tool"]["kind"], "INTEGRATION");
}

#[tokio::test]
async fn tool_kind_is_mcp_server_for_mcp_prefix() {
    let fixture =
        schema_with_seeded_catalog("acme", vec![mcp_tool("fetcher", "Fetch content")], vec![])
            .await;

    let data = execute_json(
        &fixture.schema,
        r#"{
            tool(id: "mcp:fetcher") { id kind }
        }"#,
    )
    .await;

    assert_eq!(data["tool"]["id"], "mcp:fetcher");
    assert_eq!(data["tool"]["kind"], "MCP_SERVER");
}

#[tokio::test]
async fn tool_provider_is_null_for_builtins() {
    let fixture = schema_with_seeded_catalog("acme", builtin_tools(), vec![]).await;

    let data = execute_json(
        &fixture.schema,
        r#"{
            shell: tool(id: "shell:exec") { provider }
            javascript: tool(id: "javascript") { provider }
            python: tool(id: "python") { provider }
        }"#,
    )
    .await;

    assert_eq!(data["shell"]["provider"], Value::Null);
    assert_eq!(data["javascript"]["provider"], Value::Null);
    assert_eq!(data["python"]["provider"], Value::Null);
}

#[tokio::test]
async fn tool_provider_equals_integration_name_for_integrations() {
    let fixture = schema_with_seeded_catalog(
        "acme",
        vec![integration_tool("slack", "Post messages to Slack")],
        vec![],
    )
    .await;

    let data = execute_json(
        &fixture.schema,
        r#"{
            tool(id: "integration:slack") { provider }
        }"#,
    )
    .await;

    assert_eq!(data["tool"]["provider"], "slack");
}

#[tokio::test]
async fn tool_provider_equals_server_name_for_mcp() {
    let fixture =
        schema_with_seeded_catalog("acme", vec![mcp_tool("fetcher", "Fetch content")], vec![])
            .await;

    let data = execute_json(
        &fixture.schema,
        r#"{
            tool(id: "mcp:fetcher") { provider }
        }"#,
    )
    .await;

    assert_eq!(data["tool"]["provider"], "fetcher");
}

#[tokio::test]
async fn available_tools_returns_three_builtin_capability_tools() {
    let fixture = schema_with_seeded_catalog("acme", builtin_tools(), vec![]).await;

    let data = execute_json(&fixture.schema, r#"{ availableTools { id } }"#).await;

    assert_eq!(
        tool_ids(&data["availableTools"]),
        vec!["javascript", "python", "shell:exec"]
    );
}

#[tokio::test]
async fn available_tools_returns_one_tool_per_registered_integration() {
    let mut tools = builtin_tools();
    tools.push(integration_tool("slack", "Post messages to Slack"));
    tools.push(integration_tool("github", "Open GitHub issues"));

    let fixture = schema_with_seeded_catalog("acme", tools, vec![]).await;
    let data = execute_json(&fixture.schema, r#"{ availableTools { id } }"#).await;

    let ids = data["availableTools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        ids.iter()
            .filter(|id| id.starts_with("integration:"))
            .cloned()
            .collect::<Vec<_>>(),
        vec!["integration:github", "integration:slack"]
    );
}

#[tokio::test]
async fn available_tools_returns_one_tool_per_configured_mcp_server() {
    let mut tools = builtin_tools();
    tools.push(mcp_tool("fetcher", "Fetch content"));
    tools.push(mcp_tool("planner", "Plan tasks"));

    let fixture = schema_with_seeded_catalog("acme", tools, vec![]).await;
    let data = execute_json(&fixture.schema, r#"{ availableTools { id } }"#).await;

    let ids = data["availableTools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        ids.iter()
            .filter(|id| id.starts_with("mcp:"))
            .cloned()
            .collect::<Vec<_>>(),
        vec!["mcp:fetcher", "mcp:planner"]
    );
}

#[tokio::test]
async fn available_tools_ordering_is_builtins_then_integrations_then_mcp_alphabetical() {
    let tools = vec![
        mcp_tool("zfetch", "Late alphabet MCP"),
        integration_tool("zeta", "Last integration"),
        builtin_tool("shell:exec", "Shell execution", "Execute shell commands"),
        mcp_tool("abridge", "Early alphabet MCP"),
        builtin_tool("python", "Python", "Run Python scripts"),
        integration_tool("alpha", "First integration"),
        builtin_tool("javascript", "JavaScript", "Run JavaScript snippets"),
    ];
    let fixture = schema_with_seeded_catalog("acme", tools, vec![]).await;

    let data = execute_json(&fixture.schema, r#"{ availableTools { id } }"#).await;
    let ids = data["availableTools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        ids,
        vec![
            "javascript".to_owned(),
            "python".to_owned(),
            "shell:exec".to_owned(),
            "integration:alpha".to_owned(),
            "integration:zeta".to_owned(),
            "mcp:abridge".to_owned(),
            "mcp:zfetch".to_owned(),
        ]
    );
}

#[tokio::test]
async fn available_tools_does_not_leak_other_tenants_integrations() {
    let fixture = schema_with_two_tenants(
        {
            let mut tools = builtin_tools();
            tools.push(integration_tool("slack", "Tenant A integration"));
            tools
        },
        {
            let mut tools = builtin_tools();
            tools.push(integration_tool("github", "Tenant B integration"));
            tools
        },
        vec![],
        vec![],
    )
    .await;

    let data = execute_json(&fixture.schema_a, r#"{ availableTools { id } }"#).await;
    let ids = data["availableTools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();

    assert!(ids.iter().any(|id| id == "integration:slack"));
    assert!(!ids.iter().any(|id| id == "integration:github"));
}

#[tokio::test]
async fn available_tools_does_not_leak_other_tenants_mcp_servers() {
    let fixture = schema_with_two_tenants(
        {
            let mut tools = builtin_tools();
            tools.push(mcp_tool("fetcher", "Tenant A MCP"));
            tools
        },
        {
            let mut tools = builtin_tools();
            tools.push(mcp_tool("planner", "Tenant B MCP"));
            tools
        },
        vec![],
        vec![],
    )
    .await;

    let data = execute_json(&fixture.schema_a, r#"{ availableTools { id } }"#).await;
    let ids = data["availableTools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();

    assert!(ids.iter().any(|id| id == "mcp:fetcher"));
    assert!(!ids.iter().any(|id| id == "mcp:planner"));
}

#[tokio::test]
async fn tool_query_returns_tool_for_known_id() {
    let fixture = schema_with_seeded_catalog(
        "acme",
        vec![integration_tool("slack", "Post messages to Slack")],
        vec![],
    )
    .await;

    let data = execute_json(
        &fixture.schema,
        r#"{
            tool(id: "integration:slack") {
                id
                kind
                name
                description
                provider
                inputSchema
            }
        }"#,
    )
    .await;

    assert_eq!(
        data["tool"],
        json!({
            "id": "integration:slack",
            "kind": "INTEGRATION",
            "name": "Slack",
            "description": "Post messages to Slack",
            "provider": "slack",
            "inputSchema": Value::Null,
        })
    );
}

#[tokio::test]
async fn tool_query_returns_null_for_unknown_id() {
    let fixture = schema_with_seeded_catalog("acme", builtin_tools(), vec![]).await;

    let data = execute_json(
        &fixture.schema,
        r#"{
            tool(id: "integration:unknown") { id }
        }"#,
    )
    .await;

    assert_eq!(data["tool"], Value::Null);
}

#[tokio::test]
async fn tool_query_returns_null_when_tool_belongs_to_a_different_tenant() {
    let fixture = schema_with_two_tenants(
        {
            let mut tools = builtin_tools();
            tools.push(integration_tool("slack", "Tenant A integration"));
            tools
        },
        {
            let mut tools = builtin_tools();
            tools.push(integration_tool("github", "Tenant B integration"));
            tools
        },
        vec![],
        vec![],
    )
    .await;

    let data = execute_json(
        &fixture.schema_a,
        r#"{
            tool(id: "integration:github") { id }
        }"#,
    )
    .await;

    assert_eq!(data["tool"], Value::Null);
}

#[tokio::test]
async fn agent_tools_projects_capabilities_into_structured_tools_in_order() {
    let tools = vec![
        mcp_tool("fetcher", "Fetch content"),
        builtin_tool("shell:exec", "Shell execution", "Execute shell commands"),
        integration_tool("slack", "Post messages to Slack"),
    ];
    let fixture = schema_with_seeded_catalog(
        "acme",
        tools,
        vec![AgentSeed {
            name: "ordered-agent",
            capabilities: vec![
                "integration:slack".to_owned(),
                "shell:exec".to_owned(),
                "mcp:fetcher".to_owned(),
            ],
        }],
    )
    .await;

    let data = execute_json(
        &fixture.schema,
        format!(
            r#"{{
                agent(id: "{}") {{
                    tools {{
                        id
                        kind
                        provider
                    }}
                }}
            }}"#,
            fixture.agents[0].id.as_str()
        ),
    )
    .await;

    // Alphabetical (catalog: ORDER BY capability ASC).
    assert_eq!(
        data["agent"]["tools"],
        json!([
            {
                "id": "integration:slack",
                "kind": "INTEGRATION",
                "provider": "slack"
            },
            {
                "id": "mcp:fetcher",
                "kind": "MCP_SERVER",
                "provider": "fetcher"
            },
            {
                "id": "shell:exec",
                "kind": "BUILTIN_CAPABILITY",
                "provider": Value::Null
            }
        ])
    );
}

#[tokio::test]
async fn agent_tools_drops_unresolvable_capability_strings() {
    let tools = vec![
        builtin_tool("shell:exec", "Shell execution", "Execute shell commands"),
        builtin_tool("python", "Python", "Run Python scripts"),
        integration_tool("slack", "Post messages to Slack"),
    ];
    let fixture = schema_with_seeded_catalog(
        "acme",
        tools,
        vec![AgentSeed {
            name: "partial-agent",
            capabilities: vec![
                "shell:exec".to_owned(),
                "unknown:capability".to_owned(),
                "integration:slack".to_owned(),
                "mcp:missing".to_owned(),
                "python".to_owned(),
            ],
        }],
    )
    .await;

    let data = execute_json(
        &fixture.schema,
        format!(
            r#"{{
                agent(id: "{}") {{
                    tools {{ id }}
                }}
            }}"#,
            fixture.agents[0].id.as_str()
        ),
    )
    .await;

    // Alphabetical: integration:slack, mcp:missing (DROPPED — not in catalog),
    // python, shell:exec, unknown:capability (DROPPED).
    assert_eq!(
        data["agent"]["tools"],
        json!([
            { "id": "integration:slack" },
            { "id": "python" },
            { "id": "shell:exec" }
        ])
    );
}

#[tokio::test]
async fn agent_with_empty_capabilities_has_empty_tools() {
    let fixture = schema_with_seeded_catalog(
        "acme",
        builtin_tools(),
        vec![AgentSeed {
            name: "empty-agent",
            capabilities: vec![],
        }],
    )
    .await;

    let data = execute_json(
        &fixture.schema,
        format!(
            r#"{{
                agent(id: "{}") {{
                    capabilities
                    tools {{ id }}
                }}
            }}"#,
            fixture.agents[0].id.as_str()
        ),
    )
    .await;

    assert_eq!(data["agent"]["capabilities"], json!([]));
    assert_eq!(data["agent"]["tools"], json!([]));
}

#[tokio::test]
async fn agent_tools_length_equals_capabilities_length_when_all_resolve() {
    let tools = vec![
        builtin_tool("shell:exec", "Shell execution", "Execute shell commands"),
        integration_tool("slack", "Post messages to Slack"),
        mcp_tool("fetcher", "Fetch content"),
    ];
    let fixture = schema_with_seeded_catalog(
        "acme",
        tools,
        vec![AgentSeed {
            name: "all-resolve-agent",
            capabilities: vec![
                "shell:exec".to_owned(),
                "integration:slack".to_owned(),
                "mcp:fetcher".to_owned(),
            ],
        }],
    )
    .await;

    let data = execute_json(
        &fixture.schema,
        format!(
            r#"{{
                agent(id: "{}") {{
                    capabilities
                    tools {{ id }}
                }}
            }}"#,
            fixture.agents[0].id.as_str()
        ),
    )
    .await;

    // The catalog table is `PRIMARY KEY (agent_id, capability)`, so duplicates
    // collapse server-side; for an agent whose every distinct capability has
    // a matching Tool, tools.len() == capabilities.len() (alphabetical).
    assert_eq!(data["agent"]["capabilities"].as_array().unwrap().len(), 3);
    assert_eq!(data["agent"]["tools"].as_array().unwrap().len(), 3);
    assert_eq!(
        data["agent"]["tools"],
        json!([
            { "id": "integration:slack" },
            { "id": "mcp:fetcher" },
            { "id": "shell:exec" }
        ])
    );
}
