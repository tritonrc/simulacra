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
    simulacra_catalog::Skill,
    simulacra_catalog::Skill,
    simulacra_catalog::MemoryPool,
    simulacra_catalog::Agent,
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
                name: "alpha",
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
                name: "beta",
                description: Some("beta description"),
                body: "beta body",
                metadata: Some(&json!({"tier": "silver"})),
            },
        )
        .await
        .unwrap();

    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "existing",
                description: Some("existing description"),
                system_prompt: "existing prompt",
                model: "gpt-test",
                max_turns: Some(42),
                max_tokens: Some(4096),
                memory_pool_id: Some(&pool.id),
                skill_ids: std::slice::from_ref(&skill_a.id),
                capabilities: &["mcp:fetcher:*".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

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

    (schema, tenant, skill_a, skill_b, pool, agent)
}

#[tokio::test]
async fn create_agent_returns_full_node() {
    let (schema, _tenant, skill_a, _skill_b, pool, _agent) = schema_with_seeded_catalog().await;
    let mutation = format!(
        r#"mutation {{
            createAgent(input: {{
                name: "newbie"
                description: "new description"
                systemPrompt: "prompt"
                model: "gpt-test"
                maxTurns: 12
                maxTokens: 2048
                skillIds: ["{}"]
                capabilities: ["mcp:fetcher:*"]
                memoryPoolId: "{}"
            }}) {{
                id
                name
                description
                systemPrompt
                model
                maxTurns
                maxTokens
                capabilities
                skills {{ id name }}
                memoryPool {{ id name }}
            }}
        }}"#,
        skill_a.id.as_str(),
        pool.id.as_str()
    );

    let response = schema.execute(mutation).await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["createAgent"]["name"], "newbie");
    assert_eq!(data["createAgent"]["skills"][0]["id"], skill_a.id.as_str());
    assert_eq!(data["createAgent"]["memoryPool"]["id"], pool.id.as_str());
}

#[tokio::test]
async fn create_agent_duplicate_name_returns_conflict_code() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let mutation = r#"mutation {
        createAgent(input: {
            name: "existing"
            systemPrompt: "prompt"
            model: "gpt-test"
            skillIds: []
            capabilities: []
        }) { id }
    }"#;

    let response = schema.execute(mutation).await;
    assert!(!response.errors.is_empty());
    assert_eq!(
        response.errors[0]
            .extensions
            .as_ref()
            .unwrap()
            .get("code")
            .unwrap()
            .clone()
            .into_json()
            .unwrap(),
        json!("CONFLICT")
    );
}

#[tokio::test]
async fn create_agent_unknown_skill_id_returns_validation_code() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"mutation {
                createAgent(input: {
                    name: "ghost-skill"
                    systemPrompt: "prompt"
                    model: "gpt-test"
                    skillIds: ["missing-skill"]
                    capabilities: []
                }) { id }
            }"#,
        )
        .await;

    assert!(!response.errors.is_empty());
    assert_eq!(
        response.errors[0]
            .extensions
            .as_ref()
            .unwrap()
            .get("code")
            .unwrap()
            .clone()
            .into_json()
            .unwrap(),
        json!("VALIDATION")
    );
}

#[tokio::test]
async fn create_agent_unknown_memory_pool_id_returns_validation_code() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"mutation {
                createAgent(input: {
                    name: "ghost-pool"
                    systemPrompt: "prompt"
                    model: "gpt-test"
                    skillIds: []
                    capabilities: []
                    memoryPoolId: "missing-pool"
                }) { id }
            }"#,
        )
        .await;

    assert!(!response.errors.is_empty());
    assert_eq!(
        response.errors[0]
            .extensions
            .as_ref()
            .unwrap()
            .get("code")
            .unwrap()
            .clone()
            .into_json()
            .unwrap(),
        json!("VALIDATION")
    );
}

#[tokio::test]
async fn update_agent_with_null_skill_ids_preserves_existing_skills() {
    let (schema, _tenant, skill_a, _skill_b, _pool, agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(format!(
            r#"mutation {{
                updateAgent(id: "{}", input: {{
                    description: "patched"
                    skillIds: null
                }}) {{
                    id
                    description
                    skills {{ id }}
                }}
            }}"#,
            agent.id.as_str()
        ))
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["updateAgent"]["description"], "patched");
    assert_eq!(
        data["updateAgent"]["skills"],
        json!([{ "id": skill_a.id.as_str() }])
    );
}

#[tokio::test]
async fn update_agent_omitting_skill_ids_preserves_existing_skills() {
    // B4: contract is "absent OR null → no change; empty → clear". The null
    // case is covered by the test above; this covers absent.
    let (schema, _tenant, skill_a, _skill_b, _pool, agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(format!(
            r#"mutation {{
                updateAgent(id: "{}", input: {{
                    systemPrompt: "p2"
                }}) {{
                    id
                    systemPrompt
                    skills {{ id }}
                }}
            }}"#,
            agent.id.as_str()
        ))
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["updateAgent"]["systemPrompt"], "p2");
    assert_eq!(
        data["updateAgent"]["skills"],
        json!([{ "id": skill_a.id.as_str() }]),
        "omitting skillIds must leave the existing skill set intact"
    );
}

#[tokio::test]
async fn update_agent_with_empty_skill_ids_clears_skills() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(format!(
            r#"mutation {{
                updateAgent(id: "{}", input: {{ skillIds: [] }}) {{
                    id
                    skills {{ id }}
                }}
            }}"#,
            agent.id.as_str()
        ))
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["updateAgent"]["skills"], json!([]));
}

#[tokio::test]
async fn update_agent_with_replacement_skill_ids_replaces_set() {
    let (schema, _tenant, _skill_a, skill_b, _pool, agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(format!(
            r#"mutation {{
                updateAgent(id: "{}", input: {{ skillIds: ["{}"] }}) {{
                    id
                    skills {{ id name }}
                }}
            }}"#,
            agent.id.as_str(),
            skill_b.id.as_str()
        ))
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(
        data["updateAgent"]["skills"],
        json!([{ "id": skill_b.id.as_str(), "name": "beta" }])
    );
}

#[tokio::test]
async fn delete_agent_returns_true_and_makes_agent_unqueryable() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, agent) = schema_with_seeded_catalog().await;
    let delete = schema
        .execute(format!(
            r#"mutation {{
                deleteAgent(id: "{}")
            }}"#,
            agent.id.as_str()
        ))
        .await;
    assert!(delete.errors.is_empty(), "{:?}", delete.errors);
    assert_eq!(delete.data.into_json().unwrap()["deleteAgent"], json!(true));

    let query = schema
        .execute(format!(
            r#"{{
                agent(id: "{}") {{ id }}
            }}"#,
            agent.id.as_str()
        ))
        .await;
    assert!(query.errors.is_empty(), "{:?}", query.errors);
    assert!(query.data.into_json().unwrap()["agent"].is_null());
}

#[tokio::test]
async fn delete_agent_does_not_cascade_to_skill_rows() {
    // B6: deleteAgent must clear the agent_skills join table but must NOT
    // delete the underlying skill rows — those are independently owned by the
    // tenant and may be referenced by other agents.
    let (schema, _tenant, skill_a, _skill_b, _pool, agent) = schema_with_seeded_catalog().await;

    let delete = schema
        .execute(format!(
            r#"mutation {{ deleteAgent(id: "{}") }}"#,
            agent.id.as_str()
        ))
        .await;
    assert!(delete.errors.is_empty(), "{:?}", delete.errors);
    assert_eq!(delete.data.into_json().unwrap()["deleteAgent"], json!(true));

    let query = schema
        .execute(format!(
            r#"{{
                skill(id: "{}") {{ id name body }}
            }}"#,
            skill_a.id.as_str()
        ))
        .await;
    assert!(query.errors.is_empty(), "{:?}", query.errors);
    let data = query.data.into_json().unwrap();
    assert_eq!(data["skill"]["id"], skill_a.id.as_str());
    assert_eq!(data["skill"]["name"], "alpha");
    assert_eq!(data["skill"]["body"], "alpha body");
}

#[tokio::test]
async fn create_skill_returns_full_node() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"mutation {
                createSkill(input: {
                    name: "gamma"
                    description: "gamma description"
                    body: "gamma body"
                    metadata: { addedBy: "tests" }
                }) {
                    id
                    name
                    description
                    body
                    metadata
                }
            }"#,
        )
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["createSkill"]["name"], "gamma");
    assert_eq!(data["createSkill"]["metadata"]["addedBy"], "tests");
}

#[tokio::test]
async fn create_skill_duplicate_name_returns_conflict_code() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"mutation {
                createSkill(input: {
                    name: "alpha"
                    description: "dup"
                    body: "dup"
                }) { id }
            }"#,
        )
        .await;

    assert!(!response.errors.is_empty());
    assert_eq!(
        response.errors[0]
            .extensions
            .as_ref()
            .unwrap()
            .get("code")
            .unwrap()
            .clone()
            .into_json()
            .unwrap(),
        json!("CONFLICT")
    );
}

#[tokio::test]
async fn update_skill_returns_full_node() {
    let (schema, _tenant, skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(format!(
            r#"mutation {{
                updateSkill(id: "{}", input: {{
                    name: "alpha-v2"
                    description: "updated"
                    body: "updated body"
                }}) {{
                    id
                    name
                    description
                    body
                }}
            }}"#,
            skill_a.id.as_str()
        ))
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["updateSkill"]["name"], "alpha-v2");
    assert_eq!(data["updateSkill"]["body"], "updated body");
}

#[tokio::test]
async fn delete_skill_returns_true_and_makes_it_unqueryable() {
    let (schema, _tenant, skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let delete = schema
        .execute(format!(
            r#"mutation {{
                deleteSkill(id: "{}")
            }}"#,
            skill_a.id.as_str()
        ))
        .await;
    assert!(delete.errors.is_empty(), "{:?}", delete.errors);
    assert_eq!(delete.data.into_json().unwrap()["deleteSkill"], json!(true));

    let query = schema
        .execute(format!(
            r#"{{
                skill(id: "{}") {{ id }}
            }}"#,
            skill_a.id.as_str()
        ))
        .await;
    assert!(query.errors.is_empty(), "{:?}", query.errors);
    assert!(query.data.into_json().unwrap()["skill"].is_null());
}

#[tokio::test]
async fn create_memory_pool_returns_full_node() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"mutation {
                createMemoryPool(input: {
                    name: "analytics"
                    embeddingModel: "embed-v2"
                    config: { path: "/tmp/analytics" }
                }) {
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
    assert_eq!(data["createMemoryPool"]["name"], "analytics");
    assert_eq!(data["createMemoryPool"]["config"]["path"], "/tmp/analytics");
}

#[tokio::test]
async fn create_memory_pool_duplicate_name_returns_conflict_code() {
    let (schema, _tenant, _skill_a, _skill_b, _pool, _agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(
            r#"mutation {
                createMemoryPool(input: {
                    name: "default"
                    embeddingModel: "embed-v2"
                    config: { path: "/tmp/default" }
                }) { id }
            }"#,
        )
        .await;

    assert!(!response.errors.is_empty());
    assert_eq!(
        response.errors[0]
            .extensions
            .as_ref()
            .unwrap()
            .get("code")
            .unwrap()
            .clone()
            .into_json()
            .unwrap(),
        json!("CONFLICT")
    );
}

#[tokio::test]
async fn update_memory_pool_returns_full_node() {
    let (schema, _tenant, _skill_a, _skill_b, pool, _agent) = schema_with_seeded_catalog().await;
    let response = schema
        .execute(format!(
            r#"mutation {{
                updateMemoryPool(id: "{}", input: {{
                    name: "default-v2"
                    embeddingModel: "embed-v3"
                    config: {{ path: "/tmp/v2" }}
                }}) {{
                    id
                    name
                    embeddingModel
                    config
                }}
            }}"#,
            pool.id.as_str()
        ))
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["updateMemoryPool"]["name"], "default-v2");
    assert_eq!(data["updateMemoryPool"]["config"]["path"], "/tmp/v2");
}

#[tokio::test]
async fn delete_memory_pool_returns_true_and_makes_it_unqueryable() {
    let (schema, _tenant, _skill_a, _skill_b, pool, _agent) = schema_with_seeded_catalog().await;
    let delete = schema
        .execute(format!(
            r#"mutation {{
                deleteMemoryPool(id: "{}")
            }}"#,
            pool.id.as_str()
        ))
        .await;
    assert!(delete.errors.is_empty(), "{:?}", delete.errors);
    assert_eq!(
        delete.data.into_json().unwrap()["deleteMemoryPool"],
        json!(true)
    );

    let query = schema
        .execute(format!(
            r#"{{
                memoryPool(id: "{}") {{ id }}
            }}"#,
            pool.id.as_str()
        ))
        .await;
    assert!(query.errors.is_empty(), "{:?}", query.errors);
    assert!(query.data.into_json().unwrap()["memoryPool"].is_null());
}

#[tokio::test]
async fn update_memory_pool_with_null_embedding_model_clears_field() {
    // W7: a literal null on `embeddingModel` must clear the field, not be
    // treated as "no change".
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
                name: "p1",
                embedding_model: Some("model-a"),
                config: &json!({"k": "v"}),
            },
        )
        .await
        .unwrap();

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

    let response = schema
        .execute(format!(
            r#"mutation {{
                updateMemoryPool(id: "{}", input: {{ embeddingModel: null }}) {{
                    id
                    embeddingModel
                }}
            }}"#,
            pool.id.as_str()
        ))
        .await;

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert!(
        data["updateMemoryPool"]["embeddingModel"].is_null(),
        "expected embeddingModel to be null after update, got: {data}"
    );
}

#[tokio::test]
async fn update_skill_rename_to_existing_name_returns_conflict_code() {
    // W8: renaming skill "b" → "a" must collide with the existing "a" and
    // surface as the GraphQL CONFLICT extension code.
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = catalog
        .tenants()
        .create("acme", Some("Acme"))
        .await
        .unwrap();
    let _skill_a = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "a",
                description: None,
                body: "body-a",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let skill_b = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "b",
                description: None,
                body: "body-b",
                metadata: None,
            },
        )
        .await
        .unwrap();

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

    let response = schema
        .execute(format!(
            r#"mutation {{
                updateSkill(id: "{}", input: {{ name: "a" }}) {{ id name }}
            }}"#,
            skill_b.id.as_str()
        ))
        .await;

    assert!(
        !response.errors.is_empty(),
        "expected errors when renaming to a colliding name"
    );
    assert_eq!(
        response.errors[0]
            .extensions
            .as_ref()
            .unwrap()
            .get("code")
            .unwrap()
            .clone()
            .into_json()
            .unwrap(),
        json!("CONFLICT")
    );
}
