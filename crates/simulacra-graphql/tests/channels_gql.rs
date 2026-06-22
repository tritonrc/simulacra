//! S046 Layer 2 — GraphQL channel surface coverage.

use std::sync::Arc;

use async_graphql::{EmptySubscription, Schema};
use serde_json::{Value, json};
use simulacra_catalog::repo::{
    AgentFileRepository, AgentRepository, ChannelRepository, MemoryPoolRepository, SkillRepository,
    TenantRepository,
};
use simulacra_catalog::{Catalog, Channel, ChannelKind, NewAgent, NewChannel};
use simulacra_graphql::context::{AuthenticatedPrincipal, GraphQLContext};
use simulacra_graphql::schema::{MutationRoot, QueryRoot, SimulacraSchema};

#[allow(dead_code)] // tenant_b is referenced indirectly through schema_b/channel_b.
struct Seeded {
    schema_a: SimulacraSchema,
    schema_b: SimulacraSchema,
    tenant_a: simulacra_catalog::Tenant,
    tenant_b: simulacra_catalog::Tenant,
    agent_with_channels: simulacra_catalog::Agent,
    agent_without_channels: simulacra_catalog::Agent,
    channels_a: Vec<Channel>,
    channel_b: Channel,
}

fn build_schema(
    tenant: &simulacra_catalog::Tenant,
    agents: Arc<dyn AgentRepository>,
    skills: Arc<dyn SkillRepository>,
    pools: Arc<dyn MemoryPoolRepository>,
    channels: Arc<dyn ChannelRepository>,
    agent_files: Arc<dyn AgentFileRepository>,
) -> SimulacraSchema {
    Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(agents)
    .data(skills)
    .data(pools)
    .data(channels)
    .data(agent_files)
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant.namespace.clone(),
            subject: "test-user".to_owned(),
        },
    })
    .finish()
}

async fn create_channel(
    catalog: &Catalog,
    tenant: &simulacra_catalog::Tenant,
    name: &str,
    kind: ChannelKind,
    config: Value,
) -> Channel {
    catalog
        .channels()
        .create(
            &tenant.id,
            NewChannel {
                name,
                kind,
                config: Some(&config),
            },
        )
        .await
        .unwrap()
}

async fn seed() -> Seeded {
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

    let support = create_channel(
        &catalog,
        &tenant_a,
        "support-inbox",
        ChannelKind::Slack,
        json!({"webhookUrl": "https://hooks.example.com/T1", "verified": true, "tags": ["prod"]}),
    )
    .await;
    let incidents = create_channel(
        &catalog,
        &tenant_a,
        "incidents",
        ChannelKind::Webhook,
        json!({"path": "/hooks/incidents"}),
    )
    .await;
    let manual = create_channel(
        &catalog,
        &tenant_a,
        "manual-runs",
        ChannelKind::Manual,
        json!({}),
    )
    .await;

    let channel_b = create_channel(
        &catalog,
        &tenant_b,
        "tenant-b-secret",
        ChannelKind::Email,
        json!({"address": "ops@example.com"}),
    )
    .await;

    let skill_ids: [simulacra_catalog::SkillId; 0] = [];
    let agent_with_channels = catalog
        .agents()
        .create(
            &tenant_a.id,
            NewAgent {
                name: "agent-with-channels",
                description: Some("under test"),
                system_prompt: "system",
                model: "gpt-test",
                max_turns: Some(8),
                max_tokens: Some(2048),
                memory_pool_id: None,
                skill_ids: &skill_ids,
                capabilities: &[],
                channel_ids: &[support.id.clone(), incidents.id.clone()],
            },
        )
        .await
        .unwrap();
    let agent_without_channels = catalog
        .agents()
        .create(
            &tenant_a.id,
            NewAgent {
                name: "agent-without-channels",
                description: Some("under test"),
                system_prompt: "system",
                model: "gpt-test",
                max_turns: Some(8),
                max_tokens: Some(2048),
                memory_pool_id: None,
                skill_ids: &skill_ids,
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let agents_repo: Arc<dyn AgentRepository> = Arc::new(catalog.agents());
    let skills_repo: Arc<dyn SkillRepository> = Arc::new(catalog.skills());
    let pools_repo: Arc<dyn MemoryPoolRepository> = Arc::new(catalog.memory_pools());
    let channels_repo: Arc<dyn ChannelRepository> = Arc::new(catalog.channels());
    let files_repo: Arc<dyn AgentFileRepository> = Arc::new(catalog.agent_files());

    let schema_a = build_schema(
        &tenant_a,
        Arc::clone(&agents_repo),
        Arc::clone(&skills_repo),
        Arc::clone(&pools_repo),
        Arc::clone(&channels_repo),
        Arc::clone(&files_repo),
    );
    let schema_b = build_schema(
        &tenant_b,
        agents_repo,
        skills_repo,
        pools_repo,
        channels_repo,
        files_repo,
    );

    Seeded {
        schema_a,
        schema_b,
        tenant_a,
        tenant_b,
        agent_with_channels,
        agent_without_channels,
        channels_a: vec![support, incidents, manual],
        channel_b,
    }
}

async fn execute_ok(schema: &SimulacraSchema, query: impl Into<String>) -> Value {
    let r = schema.execute(query.into()).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    r.data.into_json().unwrap()
}

async fn execute_err(
    schema: &SimulacraSchema,
    query: impl Into<String>,
) -> Vec<async_graphql::ServerError> {
    let r = schema.execute(query.into()).await;
    assert!(
        !r.errors.is_empty(),
        "expected error, got data: {:?}",
        r.data
    );
    r.errors
}

#[tokio::test]
async fn channel_query_returns_all_fields_for_known_id() {
    let s = seed().await;
    let support = &s.channels_a[0];

    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"{{
                channel(id: "{}") {{
                    id tenantId name kind config createdAt updatedAt
                }}
            }}"#,
            support.id.as_str()
        ),
    )
    .await;
    assert_eq!(data["channel"]["id"], support.id.as_str());
    assert_eq!(data["channel"]["tenantId"], s.tenant_a.id.as_str());
    assert_eq!(data["channel"]["name"], "support-inbox");
    assert_eq!(data["channel"]["kind"], "SLACK");
    assert_eq!(data["channel"]["config"], support.config);
    assert_eq!(
        data["channel"]["createdAt"],
        support.created_at.to_rfc3339()
    );
}

#[tokio::test]
async fn channel_query_returns_null_for_unknown_id() {
    let s = seed().await;
    let data = execute_ok(&s.schema_a, r#"{ channel(id: "no-such-id") { id } }"#).await;
    assert!(data["channel"].is_null());
}

#[tokio::test]
async fn channel_query_returns_null_for_cross_tenant_id() {
    let s = seed().await;
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"{{ channel(id: "{}") {{ id }} }}"#,
            s.channel_b.id.as_str()
        ),
    )
    .await;
    assert!(data["channel"].is_null());
}

#[tokio::test]
async fn channels_connection_returns_paginated_edges_and_pageinfo() {
    let s = seed().await;
    let data = execute_ok(
        &s.schema_a,
        r#"{
            channels(page: { first: 2 }) {
                edges { cursor node { id name kind } }
                pageInfo { hasNextPage hasPreviousPage startCursor endCursor }
            }
        }"#,
    )
    .await;
    let edges = data["channels"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2);
    assert_eq!(data["channels"]["pageInfo"]["hasNextPage"], json!(true));
    assert!(data["channels"]["pageInfo"]["startCursor"].is_string());
    assert!(data["channels"]["pageInfo"]["endCursor"].is_string());
}

#[tokio::test]
async fn channels_connection_filters_by_name_contains() {
    let s = seed().await;
    let data = execute_ok(
        &s.schema_a,
        r#"{
            channels(filter: { nameContains: "incident" }) {
                edges { node { name } }
            }
        }"#,
    )
    .await;
    let edges = data["channels"]["edges"].as_array().unwrap();
    let names: Vec<&str> = edges
        .iter()
        .map(|e| e["node"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"incidents"));
    assert!(!names.contains(&"manual-runs"));
}

#[tokio::test]
async fn create_channel_round_trips_config_json() {
    let s = seed().await;
    let data = execute_ok(
        &s.schema_a,
        r#"mutation {
            createChannel(input: {
                name: "ops-room",
                kind: TEAMS,
                config: { teamId: "T-99", region: "us-west" }
            }) {
                id name kind config
            }
        }"#,
    )
    .await;
    assert_eq!(data["createChannel"]["name"], "ops-room");
    assert_eq!(data["createChannel"]["kind"], "TEAMS");
    assert_eq!(
        data["createChannel"]["config"],
        json!({"teamId": "T-99", "region": "us-west"})
    );
}

#[tokio::test]
async fn create_channel_with_duplicate_name_returns_conflict_error() {
    let s = seed().await;
    let errs = execute_err(
        &s.schema_a,
        r#"mutation {
            createChannel(input: { name: "support-inbox", kind: SLACK }) {
                id
            }
        }"#,
    )
    .await;
    let msg = errs[0].message.to_lowercase();
    assert!(
        msg.contains("conflict") || msg.contains("already exists"),
        "expected conflict-style error, got {msg:?}"
    );
}

#[tokio::test]
async fn update_channel_round_trips_name() {
    let s = seed().await;
    let support = &s.channels_a[0];
    execute_ok(
        &s.schema_a,
        format!(
            r#"mutation {{
                updateChannel(id: "{}", input: {{ name: "support-renamed" }}) {{ id name }}
            }}"#,
            support.id.as_str()
        ),
    )
    .await;
    let data = execute_ok(
        &s.schema_a,
        format!(r#"{{ channel(id: "{}") {{ name }} }}"#, support.id.as_str()),
    )
    .await;
    assert_eq!(data["channel"]["name"], "support-renamed");
}

#[tokio::test]
async fn update_channel_round_trips_config_json() {
    let s = seed().await;
    let incidents = &s.channels_a[1];
    execute_ok(
        &s.schema_a,
        format!(
            r#"mutation {{
                updateChannel(id: "{}", input: {{ config: {{ rotation: "follow-the-sun" }} }}) {{ id }}
            }}"#,
            incidents.id.as_str()
        ),
    )
    .await;
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"{{ channel(id: "{}") {{ config }} }}"#,
            incidents.id.as_str()
        ),
    )
    .await;
    assert_eq!(
        data["channel"]["config"],
        json!({"rotation": "follow-the-sun"})
    );
}

#[tokio::test]
async fn delete_channel_returns_true_and_subsequent_get_returns_null() {
    let s = seed().await;
    let manual = &s.channels_a[2];
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"mutation {{ deleteChannel(id: "{}") }}"#,
            manual.id.as_str()
        ),
    )
    .await;
    assert_eq!(data["deleteChannel"], json!(true));
    let data = execute_ok(
        &s.schema_a,
        format!(r#"{{ channel(id: "{}") {{ id }} }}"#, manual.id.as_str()),
    )
    .await;
    assert!(data["channel"].is_null());
}

#[tokio::test]
async fn delete_channel_returns_false_for_unknown_id() {
    let s = seed().await;
    let data = execute_ok(
        &s.schema_a,
        r#"mutation { deleteChannel(id: "no-such-id") }"#,
    )
    .await;
    assert_eq!(data["deleteChannel"], json!(false));
}

#[tokio::test]
async fn delete_channel_returns_false_for_cross_tenant_id_and_does_not_delete() {
    let s = seed().await;
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"mutation {{ deleteChannel(id: "{}") }}"#,
            s.channel_b.id.as_str()
        ),
    )
    .await;
    assert_eq!(data["deleteChannel"], json!(false));
    // Re-query under tenant B — must still exist.
    let data = execute_ok(
        &s.schema_b,
        format!(
            r#"{{ channel(id: "{}") {{ id }} }}"#,
            s.channel_b.id.as_str()
        ),
    )
    .await;
    assert_eq!(data["channel"]["id"], s.channel_b.id.as_str());
}

#[tokio::test]
async fn agent_channels_returns_bound_channels_in_created_at_id_order() {
    let s = seed().await;
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{
                    channels {{ id name kind }}
                }}
            }}"#,
            s.agent_with_channels.id.as_str()
        ),
    )
    .await;
    let chans = data["agent"]["channels"].as_array().unwrap();
    let names: Vec<&str> = chans.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["support-inbox", "incidents"]);
}

#[tokio::test]
async fn agent_channels_returns_empty_list_when_agent_has_none() {
    let s = seed().await;
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{
                    channels {{ id }}
                }}
            }}"#,
            s.agent_without_channels.id.as_str()
        ),
    )
    .await;
    assert_eq!(data["agent"]["channels"], json!([]));
}

#[tokio::test]
async fn update_agent_with_channel_ids_replaces_binding_atomically() {
    let s = seed().await;
    let manual = &s.channels_a[2];
    execute_ok(
        &s.schema_a,
        format!(
            r#"mutation {{
                updateAgent(id: "{}", input: {{ channelIds: ["{}"] }}) {{ id }}
            }}"#,
            s.agent_with_channels.id.as_str(),
            manual.id.as_str()
        ),
    )
    .await;
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{ channels {{ id name }} }}
            }}"#,
            s.agent_with_channels.id.as_str()
        ),
    )
    .await;
    let chans = data["agent"]["channels"].as_array().unwrap();
    let names: Vec<&str> = chans.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["manual-runs"]);
}

#[tokio::test]
async fn update_agent_without_channel_ids_leaves_binding_unchanged() {
    let s = seed().await;
    execute_ok(
        &s.schema_a,
        format!(
            r#"mutation {{
                updateAgent(id: "{}", input: {{ systemPrompt: "new prompt" }}) {{ id }}
            }}"#,
            s.agent_with_channels.id.as_str()
        ),
    )
    .await;
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"{{
                agent(id: "{}") {{ channels {{ name }} }}
            }}"#,
            s.agent_with_channels.id.as_str()
        ),
    )
    .await;
    let chans = data["agent"]["channels"].as_array().unwrap();
    assert_eq!(chans.len(), 2, "channels should remain bound");
}

#[tokio::test]
async fn update_agent_with_foreign_tenant_channel_returns_validation_error() {
    let s = seed().await;
    let errs = execute_err(
        &s.schema_a,
        format!(
            r#"mutation {{
                updateAgent(id: "{}", input: {{ channelIds: ["{}"] }}) {{ id }}
            }}"#,
            s.agent_with_channels.id.as_str(),
            s.channel_b.id.as_str()
        ),
    )
    .await;
    let msg = errs[0].message.to_lowercase();
    assert!(
        msg.contains("validation") || msg.contains("not found"),
        "expected validation-style error, got {msg:?}"
    );

    // Existing channels untouched.
    let data = execute_ok(
        &s.schema_a,
        format!(
            r#"{{ agent(id: "{}") {{ channels {{ name }} }} }}"#,
            s.agent_with_channels.id.as_str()
        ),
    )
    .await;
    let chans = data["agent"]["channels"].as_array().unwrap();
    assert_eq!(chans.len(), 2);
}
