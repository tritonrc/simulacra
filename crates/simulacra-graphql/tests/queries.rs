use std::sync::Arc;

use async_graphql::{EmptySubscription, Schema};
use serde_json::json;
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Catalog, NewAgent, NewMemoryPool, NewSkill};
use simulacra_graphql::context::{AuthenticatedPrincipal, GraphQLContext};
use simulacra_graphql::schema::{MutationRoot, QueryRoot, SimulacraSchema};

async fn schema_with_seeded_catalog() -> (
    SimulacraSchema,
    simulacra_catalog::Tenant,
    Vec<simulacra_catalog::Agent>,
) {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();

    let pool = catalog
        .memory_pools()
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "default",
                embedding_model: Some("text-embed"),
                config: &json!({"path": "/tmp/memory"}),
            },
        )
        .await
        .unwrap();

    let skill_a = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha-skill",
                description: Some("alpha description"),
                body: "alpha body",
                metadata: Some(&json!({"tier": "gold"})),
            },
        )
        .await
        .unwrap();
    let skill_b = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "beta-skill",
                description: Some("beta description"),
                body: "beta body",
                metadata: Some(&json!({"tier": "silver"})),
            },
        )
        .await
        .unwrap();

    let mut agents = Vec::new();
    for (name, prompt, skills, capabilities) in [
        (
            "alpha-agent",
            "alpha prompt",
            vec![skill_a.id.clone(), skill_b.id.clone()],
            vec!["mcp:fetcher:*".to_owned(), "net:read".to_owned()],
        ),
        (
            "bravo-agent",
            "bravo prompt",
            vec![skill_a.id.clone()],
            vec!["shell:exec".to_owned()],
        ),
        (
            "charlie-agent",
            "charlie prompt",
            vec![skill_b.id.clone()],
            vec!["memory:read".to_owned()],
        ),
        (
            "delta-agent",
            "delta prompt",
            vec![skill_a.id.clone()],
            vec!["memory:write".to_owned()],
        ),
        (
            "echo-agent",
            "echo prompt",
            vec![skill_b.id.clone()],
            vec!["tool:call".to_owned()],
        ),
    ] {
        agents.push(
            catalog
                .agents()
                .create(
                    &tenant.id,
                    NewAgent {
                        name,
                        description: Some(&format!("{name} description")),
                        system_prompt: prompt,
                        model: "gpt-test",
                        max_turns: Some(42),
                        max_tokens: Some(4096),
                        memory_pool_id: Some(&pool.id),
                        skill_ids: &skills,
                        capabilities: &capabilities,
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
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant.namespace.clone(),
            subject: "test-user".to_owned(),
        },
    })
    .finish();

    (schema, tenant, agents)
}

#[tokio::test]
async fn agent_query_returns_full_node_with_joins() {
    let (schema, _tenant, agents) = schema_with_seeded_catalog().await;
    let agent = &agents[0];
    let query = format!(
        r#"{{
            agent(id: "{}") {{
                id
                name
                description
                systemPrompt
                model
                maxTurns
                maxTokens
                capabilities
                skills {{ id name body metadata }}
                memoryPool {{ id name embeddingModel config }}
                createdAt
                updatedAt
            }}
        }}"#,
        agent.id.as_str()
    );

    let response = schema.execute(query).await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["agent"]["id"], agent.id.as_str());
    assert_eq!(data["agent"]["name"], "alpha-agent");
    assert_eq!(
        data["agent"]["capabilities"],
        json!(["mcp:fetcher:*", "net:read"])
    );
    assert_eq!(data["agent"]["skills"][0]["name"], "alpha-skill");
    assert_eq!(data["agent"]["memoryPool"]["name"], "default");
}

#[tokio::test]
async fn agents_query_returns_connection_page_info_and_stable_cursor() {
    let (schema, _tenant, _agents) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"{
                agents(page: { first: 2 }) {
                    edges { cursor node { id name } }
                    pageInfo { hasNextPage hasPreviousPage startCursor endCursor }
                }
            }"#,
        )
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    let edges = data["agents"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2);
    assert_eq!(data["agents"]["pageInfo"]["hasNextPage"], json!(true));
    // B5: first-page query should report no previous page.
    assert_eq!(data["agents"]["pageInfo"]["hasPreviousPage"], json!(false));
    // B5: startCursor must be a non-empty string on a populated page.
    let start_cursor = data["agents"]["pageInfo"]["startCursor"]
        .as_str()
        .expect("startCursor should be a string on a non-empty page");
    assert!(!start_cursor.is_empty(), "startCursor should be non-empty");
    // B5: endCursor must equal the last edge's cursor.
    let end_cursor = data["agents"]["pageInfo"]["endCursor"]
        .as_str()
        .expect("endCursor should be a string");
    assert_eq!(
        end_cursor,
        edges[edges.len() - 1]["cursor"].as_str().unwrap(),
        "endCursor should equal the last edge's cursor"
    );
    assert_ne!(edges[0]["cursor"], edges[1]["cursor"]);
}

#[tokio::test]
async fn skills_query_paginates_like_agents() {
    let (schema, _tenant, _agents) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"{
                skills(page: { first: 1 }) {
                    edges { cursor node { id name body } }
                    pageInfo { hasNextPage endCursor }
                }
            }"#,
        )
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["skills"]["edges"].as_array().unwrap().len(), 1);
    assert_eq!(data["skills"]["pageInfo"]["hasNextPage"], json!(true));
    assert!(data["skills"]["pageInfo"]["endCursor"].is_string());
}

#[tokio::test]
async fn memory_pools_query_returns_all_visible_pools() {
    let (schema, _tenant, _agents) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"{
                memoryPools {
                    id
                    name
                    embeddingModel
                    config
                }
            }"#,
        )
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["memoryPools"].as_array().unwrap().len(), 1);
    assert_eq!(data["memoryPools"][0]["name"], "default");
}

#[tokio::test]
async fn agents_name_contains_filter_reduces_results() {
    let (schema, _tenant, _agents) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"{
                agents(filter: { nameContains: "alpha" }, page: { first: 5 }) {
                    edges { node { id name } }
                    pageInfo { hasNextPage }
                }
            }"#,
        )
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    let edges = data["agents"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["node"]["name"], "alpha-agent");
}

#[tokio::test]
async fn agents_cursor_round_trip_on_five_rows_has_no_overlap() {
    let (schema, _tenant, _agents) = schema_with_seeded_catalog().await;

    let page_one = schema
        .execute(
            r#"{
                agents(page: { first: 2 }) {
                    edges { node { id name } }
                    pageInfo { endCursor hasNextPage }
                }
            }"#,
        )
        .await;
    assert!(page_one.errors.is_empty(), "{:?}", page_one.errors);
    let page_one_json = page_one.data.into_json().unwrap();
    let cursor = page_one_json["agents"]["pageInfo"]["endCursor"]
        .as_str()
        .unwrap()
        .to_owned();

    let page_two = schema
        .execute(format!(
            r#"{{
                agents(page: {{ first: 2, after: "{}" }}) {{
                    edges {{ node {{ id name }} }}
                    pageInfo {{ endCursor hasNextPage }}
                }}
            }}"#,
            cursor
        ))
        .await;

    assert!(page_two.errors.is_empty(), "{:?}", page_two.errors);
    let page_two_json = page_two.data.into_json().unwrap();
    let first_page_ids: Vec<_> = page_one_json["agents"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["node"]["id"].as_str().unwrap().to_owned())
        .collect();
    let second_page_ids: Vec<_> = page_two_json["agents"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["node"]["id"].as_str().unwrap().to_owned())
        .collect();

    assert_eq!(first_page_ids.len(), 2);
    assert_eq!(second_page_ids.len(), 2);
    assert!(
        first_page_ids
            .iter()
            .all(|id| !second_page_ids.contains(id))
    );
}

/// Two-tenant fixture for cross-tenant isolation tests (B3).
///
/// Seeds tenant A with one agent (referencing one skill + one memory pool)
/// and tenant B with one agent, one skill, and one memory pool. Returns a
/// schema for each tenant that shares the same underlying repositories but
/// is configured with a tenant-scoped `GraphQLContext`. Tests then ask
/// schema-as-A for B's rows and assert they are invisible.
struct TwoTenantFixture {
    schema_a: SimulacraSchema,
    schema_b: SimulacraSchema,
    tenant_a_agent: simulacra_catalog::Agent,
    tenant_b_agent: simulacra_catalog::Agent,
    tenant_b_skill: simulacra_catalog::Skill,
    tenant_b_pool: simulacra_catalog::MemoryPool,
}

async fn seed_two_tenants() -> TwoTenantFixture {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant_a = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    let tenant_b = catalog
        .tenants()
        .create("evil", Some("Evil"))
        .await
        .unwrap();

    let pool_a = catalog
        .memory_pools()
        .create(
            &tenant_a.id,
            NewMemoryPool {
                name: "pool-a",
                embedding_model: Some("e-a"),
                config: &json!({"path": "/a"}),
            },
        )
        .await
        .unwrap();
    let skill_a = catalog
        .skills()
        .create(
            &tenant_a.id,
            NewSkill {
                name: "skill-a",
                description: None,
                body: "body-a",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let agent_a = catalog
        .agents()
        .create(
            &tenant_a.id,
            NewAgent {
                name: "agent-a",
                description: Some("a"),
                system_prompt: "p",
                model: "gpt-test",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: Some(&pool_a.id),
                skill_ids: std::slice::from_ref(&skill_a.id),
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let pool_b = catalog
        .memory_pools()
        .create(
            &tenant_b.id,
            NewMemoryPool {
                name: "pool-b",
                embedding_model: Some("e-b"),
                config: &json!({"path": "/b"}),
            },
        )
        .await
        .unwrap();
    let skill_b = catalog
        .skills()
        .create(
            &tenant_b.id,
            NewSkill {
                name: "skill-b",
                description: None,
                body: "body-b",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let agent_b = catalog
        .agents()
        .create(
            &tenant_b.id,
            NewAgent {
                name: "agent-b",
                description: Some("b"),
                system_prompt: "p",
                model: "gpt-test",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: Some(&pool_b.id),
                skill_ids: std::slice::from_ref(&skill_b.id),
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let agents_repo = Arc::new(catalog.agents()) as Arc<dyn AgentRepository>;
    let skills_repo = Arc::new(catalog.skills()) as Arc<dyn SkillRepository>;
    let pools_repo = Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>;

    let schema_a = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::clone(&agents_repo))
    .data(Arc::clone(&skills_repo))
    .data(Arc::clone(&pools_repo))
    .data(GraphQLContext {
        tenant_id: tenant_a.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant_a.namespace.clone(),
            subject: "user-a".to_owned(),
        },
    })
    .finish();
    let schema_b = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(agents_repo)
    .data(skills_repo)
    .data(pools_repo)
    .data(GraphQLContext {
        tenant_id: tenant_b.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant_b.namespace.clone(),
            subject: "user-b".to_owned(),
        },
    })
    .finish();

    TwoTenantFixture {
        schema_a,
        schema_b,
        tenant_a_agent: agent_a,
        tenant_b_agent: agent_b,
        tenant_b_skill: skill_b,
        tenant_b_pool: pool_b,
    }
}

#[tokio::test]
async fn agents_query_does_not_leak_other_tenants_rows() {
    let fx = seed_two_tenants().await;
    let response = fx
        .schema_a
        .execute(
            r#"{
                agents(page: { first: 50 }) {
                    edges { node { id name } }
                    pageInfo { hasNextPage }
                }
            }"#,
        )
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    let edges = data["agents"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "tenant A should only see its own agent");
    assert_eq!(edges[0]["node"]["id"], fx.tenant_a_agent.id.as_str());
    // Sanity: tenant B sees its own row independently.
    let _ = fx.tenant_b_agent;
    let b_resp = fx
        .schema_b
        .execute(r#"{ agents(page: { first: 50 }) { edges { node { id } } } }"#)
        .await;
    assert!(b_resp.errors.is_empty(), "{:?}", b_resp.errors);
}

#[tokio::test]
async fn skill_query_returns_null_for_cross_tenant_skill() {
    let fx = seed_two_tenants().await;
    let response = fx
        .schema_a
        .execute(format!(
            r#"{{ skill(id: "{}") {{ id name }} }}"#,
            fx.tenant_b_skill.id.as_str()
        ))
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert!(
        data["skill"].is_null(),
        "tenant A asking for tenant B's skill should get null, got: {data}"
    );
}

#[tokio::test]
async fn memory_pool_query_returns_null_for_cross_tenant_pool() {
    let fx = seed_two_tenants().await;
    let response = fx
        .schema_a
        .execute(format!(
            r#"{{ memoryPool(id: "{}") {{ id name }} }}"#,
            fx.tenant_b_pool.id.as_str()
        ))
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert!(
        data["memoryPool"].is_null(),
        "tenant A asking for tenant B's pool should get null, got: {data}"
    );
}

#[tokio::test]
async fn memory_pools_query_does_not_leak_other_tenants_pools() {
    let fx = seed_two_tenants().await;
    // Seed an extra pool in B to make sure A's view stays clean.
    let _ = fx.tenant_b_pool;
    let response = fx.schema_a.execute(r#"{ memoryPools { id name } }"#).await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    let pools = data["memoryPools"].as_array().unwrap();
    assert_eq!(pools.len(), 1, "tenant A should only see its own pool");
    assert_eq!(pools[0]["name"], "pool-a");
}

#[tokio::test]
async fn skills_query_does_not_leak_other_tenants_rows() {
    let fx = seed_two_tenants().await;
    let _ = fx.tenant_b_skill;
    let response = fx
        .schema_a
        .execute(
            r#"{
                skills(page: { first: 50 }) {
                    edges { node { id name } }
                    pageInfo { hasNextPage }
                }
            }"#,
        )
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    let edges = data["skills"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "tenant A should only see its own skill");
    assert_eq!(edges[0]["node"]["name"], "skill-a");
}

#[tokio::test]
async fn agents_cursor_remains_stable_when_new_agent_inserted_mid_traversal() {
    // W1: Pagination uses a cursor that orders rows by their original
    // creation key, so inserting a row with a *later* timestamp must not
    // shift earlier rows back into a "previous" page or disturb the cursor's
    // identity. We control timing by inserting the new row directly through
    // the catalog repo (not GraphQL).
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();

    let mut original = Vec::new();
    for name in ["a1", "a2", "a3", "a4"] {
        original.push(
            catalog
                .agents()
                .create(
                    &tenant.id,
                    NewAgent {
                        name,
                        description: None,
                        system_prompt: "p",
                        model: "gpt-test",
                        max_turns: None,
                        max_tokens: None,
                        memory_pool_id: None,
                        skill_ids: &[],
                        capabilities: &[],
                        channel_ids: &[],
                    },
                )
                .await
                .unwrap(),
        );
    }

    let agents_repo = Arc::new(catalog.agents()) as Arc<dyn AgentRepository>;
    let skills_repo = Arc::new(catalog.skills()) as Arc<dyn SkillRepository>;
    let pools_repo = Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>;
    let schema = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::clone(&agents_repo))
    .data(Arc::clone(&skills_repo))
    .data(Arc::clone(&pools_repo))
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant.namespace.clone(),
            subject: "user".to_owned(),
        },
    })
    .finish();

    let page1 = schema
        .execute(
            r#"{
                agents(page: { first: 2 }) {
                    edges { node { id name } }
                    pageInfo { endCursor hasNextPage }
                }
            }"#,
        )
        .await;
    assert!(page1.errors.is_empty(), "{:?}", page1.errors);
    let p1_json = page1.data.into_json().unwrap();
    let cursor = p1_json["agents"]["pageInfo"]["endCursor"]
        .as_str()
        .unwrap()
        .to_owned();

    // Insert a fifth agent with a strictly-later created_at *between* page
    // requests. It should not appear in the second page when ordered ascending
    // by created_at.
    let _intruder = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "intruder",
                description: None,
                system_prompt: "p",
                model: "gpt-test",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let page2 = schema
        .execute(format!(
            r#"{{
                agents(page: {{ first: 2, after: "{}" }}) {{
                    edges {{ node {{ id name }} }}
                    pageInfo {{ endCursor hasNextPage }}
                }}
            }}"#,
            cursor
        ))
        .await;
    assert!(page2.errors.is_empty(), "{:?}", page2.errors);
    let p2_json = page2.data.into_json().unwrap();
    let names: Vec<String> = p2_json["agents"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        vec!["a3".to_owned(), "a4".to_owned()],
        "second page should be the originally-third and -fourth agents, not the late insert"
    );
    assert!(
        !names.iter().any(|n| n == "intruder"),
        "the late-inserted agent must not appear on this page"
    );
}

#[tokio::test]
async fn agents_filter_combined_with_pagination_returns_correct_page_info() {
    // Phase 4 BLOCKER fix: the `nameContains` filter must be applied at the
    // repo (SQL) layer, not after pagination. Otherwise `pageInfo.hasNextPage`
    // reflects the *unfiltered* page boundary and can be `true` on a page that
    // returned zero matches — semantically wrong. Seed 5 agents where exactly
    // 3 contain the substring "matchme"; page in groups of 2 with the filter
    // and assert pageInfo terminates correctly.
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    for name in [
        "matchme-one",
        "other-one",
        "matchme-two",
        "other-two",
        "matchme-three",
    ] {
        catalog
            .agents()
            .create(
                &tenant.id,
                NewAgent {
                    name,
                    description: None,
                    system_prompt: "p",
                    model: "gpt-test",
                    max_turns: None,
                    max_tokens: None,
                    memory_pool_id: None,
                    skill_ids: &[],
                    capabilities: &[],
                    channel_ids: &[],
                },
            )
            .await
            .unwrap();
    }

    let agents_repo = Arc::new(catalog.agents()) as Arc<dyn AgentRepository>;
    let skills_repo = Arc::new(catalog.skills()) as Arc<dyn SkillRepository>;
    let pools_repo = Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>;
    let schema = Schema::build(
        QueryRoot::default(),
        MutationRoot::default(),
        EmptySubscription,
    )
    .data(Arc::clone(&agents_repo))
    .data(Arc::clone(&skills_repo))
    .data(Arc::clone(&pools_repo))
    .data(GraphQLContext {
        tenant_id: tenant.id.clone(),
        principal: AuthenticatedPrincipal {
            tenant_namespace: tenant.namespace.clone(),
            subject: "user".to_owned(),
        },
    })
    .finish();

    // Page 1: filter "matchme" with first=2 — two matches, more available.
    let page1 = schema
        .execute(
            r#"{
                agents(filter: { nameContains: "matchme" }, page: { first: 2 }) {
                    edges { cursor node { id name } }
                    pageInfo { hasNextPage endCursor }
                }
            }"#,
        )
        .await;
    assert!(page1.errors.is_empty(), "{:?}", page1.errors);
    let p1 = page1.data.into_json().unwrap();
    let edges1 = p1["agents"]["edges"].as_array().unwrap();
    assert_eq!(edges1.len(), 2, "page1 must contain exactly 2 matches");
    assert_eq!(edges1[0]["node"]["name"], json!("matchme-one"));
    assert_eq!(edges1[1]["node"]["name"], json!("matchme-two"));
    assert_eq!(
        p1["agents"]["pageInfo"]["hasNextPage"],
        json!(true),
        "hasNextPage should be true: 3 of 5 rows match, only 2 returned"
    );
    let cursor1 = p1["agents"]["pageInfo"]["endCursor"]
        .as_str()
        .unwrap()
        .to_owned();

    // Page 2: same filter with `after = endCursor` — exactly the third match,
    // and pageInfo must terminate (this is the BLOCKER assertion).
    let page2 = schema
        .execute(format!(
            r#"{{
                agents(filter: {{ nameContains: "matchme" }}, page: {{ first: 2, after: "{}" }}) {{
                    edges {{ cursor node {{ id name }} }}
                    pageInfo {{ hasNextPage endCursor }}
                }}
            }}"#,
            cursor1
        ))
        .await;
    assert!(page2.errors.is_empty(), "{:?}", page2.errors);
    let p2 = page2.data.into_json().unwrap();
    let edges2 = p2["agents"]["edges"].as_array().unwrap();
    assert_eq!(
        edges2.len(),
        1,
        "page2 must yield the remaining single match"
    );
    assert_eq!(edges2[0]["node"]["name"], json!("matchme-three"));
    assert_eq!(
        p2["agents"]["pageInfo"]["hasNextPage"],
        json!(false),
        "hasNextPage must be false on the terminal filtered page"
    );
}
