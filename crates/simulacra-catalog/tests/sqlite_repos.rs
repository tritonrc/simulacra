use serde_json::json;
use simulacra_catalog::models::{
    AgentPatch, MemoryPoolPatch, NewAgent, NewMemoryPool, NewSkill, PageRequest, SkillPatch,
};
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{AgentId, Catalog, CatalogError, MemoryPoolId, SkillId};

fn fresh() -> Catalog {
    Catalog::open_in_memory().unwrap()
}

#[tokio::test]
async fn tenant_create_and_get_by_namespace() {
    let catalog = fresh();
    let tenants = catalog.tenants();

    let created = tenants.create("acme", Some("Acme Corp")).await.unwrap();
    let fetched = tenants.get_by_namespace("acme").await.unwrap();

    assert_eq!(created.namespace, "acme");
    assert_eq!(fetched.id.as_str(), created.id.as_str());
}

#[tokio::test]
async fn tenant_create_duplicate_namespace_returns_conflict() {
    let catalog = fresh();
    let tenants = catalog.tenants();

    tenants.create("acme", None).await.unwrap();
    let err = tenants.create("acme", None).await.unwrap_err();

    assert!(matches!(err, CatalogError::Conflict(_)));
}

#[tokio::test]
async fn tenant_get_or_create_is_idempotent() {
    let catalog = fresh();
    let tenants = catalog.tenants();

    let a = tenants.get_or_create("default", None).await.unwrap();
    let b = tenants
        .get_or_create("default", Some("ignored"))
        .await
        .unwrap();

    assert_eq!(a.id.as_str(), b.id.as_str());
}

#[tokio::test]
async fn memory_pool_crud_round_trip() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let pools = catalog.memory_pools();

    let created = pools
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: Some("local-st-mini"),
                config: &json!({"vector_dim": 384}),
            },
        )
        .await
        .unwrap();
    assert_eq!(created.name, "shared");
    assert!(!created.id.as_str().is_empty());
    assert!(created.created_at <= created.updated_at);

    let fetched = pools.get(&tenant.id, &created.id).await.unwrap();
    assert_eq!(fetched.id.as_str(), created.id.as_str());

    let fetched_by_name = pools.get_by_name(&tenant.id, "shared").await.unwrap();
    assert_eq!(fetched_by_name.id.as_str(), created.id.as_str());

    let updated = pools
        .update(
            &tenant.id,
            &created.id,
            MemoryPoolPatch {
                name: Some("shared-v2"),
                embedding_model: Some(Some("text-embed-3-large")),
                config: Some(&json!({"vector_dim": 1536})),
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "shared-v2");
    assert_eq!(updated.config["vector_dim"], json!(1536));

    let listed = pools.list(&tenant.id).await.unwrap();
    assert_eq!(listed.len(), 1);

    pools.delete(&tenant.id, &created.id).await.unwrap();
    let err = pools.get(&tenant.id, &created.id).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn memory_pool_cross_tenant_get_returns_not_found() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();
    let pools = catalog.memory_pools();

    let pool = pools
        .create(
            &alice.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap();

    let err = pools.get(&bob.id, &pool.id).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn memory_pool_cross_tenant_get_by_name_returns_not_found() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();
    let pools = catalog.memory_pools();

    pools
        .create(
            &alice.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap();

    let err = pools.get_by_name(&bob.id, "shared").await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn memory_pool_cross_tenant_list_returns_only_visible_rows() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();
    let pools = catalog.memory_pools();

    pools
        .create(
            &alice.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap();

    let listed = pools.list(&bob.id).await.unwrap();
    assert!(listed.is_empty());
}

#[tokio::test]
async fn memory_pool_create_duplicate_name_returns_conflict() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let pools = catalog.memory_pools();

    pools
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap();

    let err = pools
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::Conflict(_)));
}

#[tokio::test]
async fn memory_pool_update_missing_id_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .memory_pools()
        .update(
            &tenant.id,
            &MemoryPoolId::new(),
            MemoryPoolPatch {
                name: Some("updated"),
                embedding_model: None,
                config: None,
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn memory_pool_delete_missing_id_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .memory_pools()
        .delete(&tenant.id, &MemoryPoolId::new())
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn skill_crud_round_trip() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skills = catalog.skills();

    let created = skills
        .create(
            &tenant.id,
            NewSkill {
                name: "summarize",
                description: Some("summaries"),
                body: "body",
                metadata: Some(&json!({"title": "Summarize"})),
            },
        )
        .await
        .unwrap();
    assert_eq!(created.name, "summarize");
    assert!(!created.id.as_str().is_empty());
    assert!(created.created_at <= created.updated_at);

    let fetched = skills.get(&tenant.id, &created.id).await.unwrap();
    assert_eq!(fetched.id.as_str(), created.id.as_str());

    let fetched_by_name = skills.get_by_name(&tenant.id, "summarize").await.unwrap();
    assert_eq!(fetched_by_name.id.as_str(), created.id.as_str());

    let updated = skills
        .update(
            &tenant.id,
            &created.id,
            SkillPatch {
                name: Some("summarize-v2"),
                description: Some(Some("updated")),
                body: Some("updated body"),
                metadata: Some(Some(&json!({"title": "Summarize v2"}))),
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "summarize-v2");
    assert_eq!(updated.body, "updated body");

    let listed = skills
        .list(&tenant.id, PageRequest::default(), None)
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 1);

    skills.delete(&tenant.id, &created.id).await.unwrap();
    let err = skills.get(&tenant.id, &created.id).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn skill_cross_tenant_returns_not_found() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();
    let skills = catalog.skills();

    let skill = skills
        .create(
            &alice.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "body",
                metadata: None,
            },
        )
        .await
        .unwrap();

    let err = skills.get(&bob.id, &skill.id).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn skill_cross_tenant_get_by_name_returns_not_found() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();
    let skills = catalog.skills();

    skills
        .create(
            &alice.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "body",
                metadata: None,
            },
        )
        .await
        .unwrap();

    let err = skills.get_by_name(&bob.id, "alpha").await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn skill_list_paginates_with_stable_cursor() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skills = catalog.skills();

    let insertion_order = ["alpha", "beta", "gamma", "delta", "epsilon"];
    let mut inserted_ids = Vec::new();
    for name in insertion_order {
        let s = skills
            .create(
                &tenant.id,
                NewSkill {
                    name,
                    description: None,
                    body: name,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        inserted_ids.push(s.id.as_str().to_owned());
    }

    let first = skills
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: None,
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.items.len(), 2);
    assert!(first.end_cursor.is_some());

    let second = skills
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: first.end_cursor.clone(),
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 2);
    assert!(second.end_cursor.is_some());

    let third = skills
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: second.end_cursor.clone(),
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(third.items.len(), 1);

    // Concatenation of all paged ids must equal the insertion order.
    let mut concatenated: Vec<String> = Vec::new();
    for s in first
        .items
        .iter()
        .chain(second.items.iter())
        .chain(third.items.iter())
    {
        concatenated.push(s.id.as_str().to_owned());
    }
    assert_eq!(concatenated, inserted_ids);

    // Concatenation of paged names must equal insertion-order names.
    let mut concatenated_names: Vec<String> = Vec::new();
    for s in first
        .items
        .iter()
        .chain(second.items.iter())
        .chain(third.items.iter())
    {
        concatenated_names.push(s.name.clone());
    }
    let expected_names: Vec<String> = insertion_order.iter().map(|s| s.to_string()).collect();
    assert_eq!(concatenated_names, expected_names);
}

#[tokio::test]
async fn skill_cross_tenant_list_returns_empty_page() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();
    let skills = catalog.skills();

    skills
        .create(
            &alice.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "body",
                metadata: None,
            },
        )
        .await
        .unwrap();

    let page = skills
        .list(&bob.id, PageRequest::default(), None)
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

#[tokio::test]
async fn skill_list_for_agent_only_returns_joined_skills() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skills = catalog.skills();

    let a = skills
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "alpha",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let _b = skills
        .create(
            &tenant.id,
            NewSkill {
                name: "beta",
                description: None,
                body: "beta",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: std::slice::from_ref(&a.id),
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let joined = skills.list_for_agent(&tenant.id, &agent.id).await.unwrap();
    assert_eq!(joined.len(), 1);
    assert_eq!(joined[0].id.as_str(), a.id.as_str());
}

#[tokio::test]
async fn skill_list_for_agent_cross_tenant_returns_no_rows() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();

    let skill = catalog
        .skills()
        .create(
            &alice.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "alpha",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let agent = catalog
        .agents()
        .create(
            &alice.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: std::slice::from_ref(&skill.id),
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let joined = catalog
        .skills()
        .list_for_agent(&bob.id, &agent.id)
        .await
        .unwrap();
    assert!(joined.is_empty());
}

#[tokio::test]
async fn agent_capabilities_returns_joined_capabilities_for_visible_agent() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &["mcp:fetcher:*".to_owned(), "net:read".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let capabilities = catalog
        .agents()
        .capabilities(&tenant.id, &agent.id)
        .await
        .unwrap();
    assert_eq!(
        capabilities,
        vec!["mcp:fetcher:*".to_owned(), "net:read".to_owned()]
    );
}

#[tokio::test]
async fn agent_capabilities_hides_cross_tenant_agent() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();

    let agent = catalog
        .agents()
        .create(
            &alice.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &["mcp:fetcher:*".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let capabilities = catalog
        .agents()
        .capabilities(&bob.id, &agent.id)
        .await
        .unwrap();
    assert!(capabilities.is_empty());
}

#[tokio::test]
async fn skill_create_duplicate_name_returns_conflict() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skills = catalog.skills();

    skills
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "body",
                metadata: None,
            },
        )
        .await
        .unwrap();

    let err = skills
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "body",
                metadata: None,
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::Conflict(_)));
}

#[tokio::test]
async fn skill_get_missing_id_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .skills()
        .get(&tenant.id, &SkillId::new())
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn skill_get_missing_name_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .skills()
        .get_by_name(&tenant.id, "missing")
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn skill_update_missing_id_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .skills()
        .update(
            &tenant.id,
            &SkillId::new(),
            SkillPatch {
                name: Some("updated"),
                description: None,
                body: None,
                metadata: None,
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn skill_delete_missing_id_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .skills()
        .delete(&tenant.id, &SkillId::new())
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn agent_create_with_skills_and_capabilities() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "summarize",
                description: None,
                body: "# Summarize",
                metadata: None,
            },
        )
        .await
        .unwrap();

    let created = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: Some("helpful"),
                system_prompt: "You are a helpful assistant.",
                model: "openai/gpt-oss-120b",
                max_turns: Some(50),
                max_tokens: Some(4096),
                memory_pool_id: None,
                skill_ids: std::slice::from_ref(&skill.id),
                capabilities: &["mcp:fetcher:*".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    assert!(!created.id.as_str().is_empty());
    assert!(created.created_at <= created.updated_at);

    let resolved = catalog
        .agents()
        .resolve(&tenant.id, "assistant")
        .await
        .unwrap();
    assert_eq!(resolved.id.as_str(), created.id.as_str());
    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.capabilities, vec!["mcp:fetcher:*"]);
}

#[tokio::test]
async fn agent_create_duplicate_name_returns_conflict() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let agents = catalog.agents();

    agents
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
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

    let err = agents
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::Conflict(_)));
}

#[tokio::test]
async fn agent_get_by_missing_name_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .agents()
        .get_by_name(&tenant.id, "missing")
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn agent_get_by_missing_id_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .agents()
        .get(&tenant.id, &AgentId::new())
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn agent_update_replaces_skills_transactionally() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skills = catalog.skills();

    let first = skills
        .create(
            &tenant.id,
            NewSkill {
                name: "one",
                description: None,
                body: "one",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let second = skills
        .create(
            &tenant.id,
            NewSkill {
                name: "two",
                description: None,
                body: "two",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: std::slice::from_ref(&first.id),
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let replacement_ids = vec![second.id.clone()];
    catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                skill_ids: Some(&replacement_ids),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let resolved = catalog
        .agents()
        .resolve(&tenant.id, "assistant")
        .await
        .unwrap();
    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.skills[0].id.as_str(), second.id.as_str());
}

#[tokio::test]
async fn agent_update_patches_only_provided_fields() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: Some("original"),
                system_prompt: "prompt",
                model: "model-a",
                max_turns: Some(10),
                max_tokens: Some(1000),
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &["mcp:fetcher:*".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let updated = catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                description: Some(Some("updated")),
                model: Some("model-b"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.description.as_deref(), Some("updated"));
    assert_eq!(updated.model, "model-b");
    assert_eq!(updated.system_prompt, "prompt");
    assert_eq!(updated.max_turns, 10);
    assert_eq!(updated.max_tokens, Some(1000));
}

#[tokio::test]
async fn agent_update_with_empty_skill_ids_clears_set() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "alpha",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: std::slice::from_ref(&skill.id),
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let empty: Vec<simulacra_catalog::SkillId> = Vec::new();
    catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                skill_ids: Some(&empty),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let resolved = catalog
        .agents()
        .resolve(&tenant.id, "assistant")
        .await
        .unwrap();
    assert!(resolved.skills.is_empty());
}

#[tokio::test]
async fn agent_update_with_null_skill_ids_preserves_set() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "alpha",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: std::slice::from_ref(&skill.id),
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                description: Some(Some("updated")),
                skill_ids: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let resolved = catalog
        .agents()
        .resolve(&tenant.id, "assistant")
        .await
        .unwrap();
    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.skills[0].id.as_str(), skill.id.as_str());
}

#[tokio::test]
async fn agent_update_nonexistent_id_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .agents()
        .update(
            &tenant.id,
            &AgentId::new(),
            AgentPatch {
                model: Some("model"),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn agent_delete_cascades_skills_and_capabilities() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "alpha",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: std::slice::from_ref(&skill.id),
                capabilities: &["mcp:fetcher:*".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    catalog
        .agents()
        .delete(&tenant.id, &agent.id)
        .await
        .unwrap();

    let get_err = catalog
        .agents()
        .get(&tenant.id, &agent.id)
        .await
        .unwrap_err();
    assert!(matches!(get_err, CatalogError::NotFound(_)));

    let joined = catalog
        .skills()
        .list_for_agent(&tenant.id, &agent.id)
        .await
        .unwrap();
    assert!(joined.is_empty());

    // The trait no longer exposes a `capabilities()` accessor (it lacked tenant
    // scoping); verify the cascade directly via raw SQL on the test connection.
    let conn = catalog.conn_for_tests();
    let guard = conn.lock().unwrap();
    let cap_count: i64 = guard
        .query_row(
            "SELECT COUNT(*) FROM agent_capabilities WHERE agent_id = ?1",
            [agent.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cap_count, 0, "agent_capabilities cascade failed");
}

#[tokio::test]
async fn agent_delete_nonexistent_id_returns_not_found() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .agents()
        .delete(&tenant.id, &AgentId::new())
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn agent_cross_tenant_get_returns_not_found() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();

    let agent = catalog
        .agents()
        .create(
            &alice.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
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

    let err = catalog.agents().get(&bob.id, &agent.id).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn agent_cross_tenant_get_by_name_returns_not_found() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();

    catalog
        .agents()
        .create(
            &alice.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
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

    let err = catalog
        .agents()
        .get_by_name(&bob.id, "assistant")
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn agent_list_paginates_with_stable_cursor() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let agents = catalog.agents();

    let insertion_order = ["alpha", "beta", "gamma", "delta", "epsilon"];
    let mut inserted_ids = Vec::new();
    for name in insertion_order {
        let a = agents
            .create(
                &tenant.id,
                NewAgent {
                    name,
                    description: None,
                    system_prompt: "prompt",
                    model: "model",
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
        inserted_ids.push(a.id.as_str().to_owned());
    }

    let first = agents
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: None,
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.items.len(), 2);
    assert!(first.end_cursor.is_some());

    let second = agents
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: first.end_cursor.clone(),
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(second.items.len(), 2);
    assert!(second.end_cursor.is_some());

    let third = agents
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: second.end_cursor.clone(),
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(third.items.len(), 1);

    let mut concatenated: Vec<String> = Vec::new();
    for a in first
        .items
        .iter()
        .chain(second.items.iter())
        .chain(third.items.iter())
    {
        concatenated.push(a.id.as_str().to_owned());
    }
    assert_eq!(concatenated, inserted_ids);

    let mut concatenated_names: Vec<String> = Vec::new();
    for a in first
        .items
        .iter()
        .chain(second.items.iter())
        .chain(third.items.iter())
    {
        concatenated_names.push(a.name.clone());
    }
    let expected_names: Vec<String> = insertion_order.iter().map(|s| s.to_string()).collect();
    assert_eq!(concatenated_names, expected_names);
}

#[tokio::test]
async fn agent_cross_tenant_list_returns_empty_page() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();

    catalog
        .agents()
        .create(
            &alice.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
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

    let page = catalog
        .agents()
        .list(&bob.id, PageRequest::default(), None)
        .await
        .unwrap();
    assert!(page.items.is_empty());
}

#[tokio::test]
async fn agent_cross_tenant_resolve_returns_not_found() {
    let catalog = fresh();
    let alice = catalog.tenants().create("alice", None).await.unwrap();
    let bob = catalog.tenants().create("bob", None).await.unwrap();

    catalog
        .agents()
        .create(
            &alice.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
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

    let err = catalog
        .agents()
        .resolve(&bob.id, "assistant")
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn resolve_returns_full_snapshot_with_joined_skills_caps_and_pool() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: Some("desc"),
                body: "body",
                metadata: Some(&json!({"x": 1})),
            },
        )
        .await
        .unwrap();
    let pool = catalog
        .memory_pools()
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: Some("embed"),
                config: &json!({"vector_dim": 384}),
            },
        )
        .await
        .unwrap();

    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: Some(20),
                max_tokens: Some(2048),
                memory_pool_id: Some(&pool.id),
                skill_ids: std::slice::from_ref(&skill.id),
                capabilities: &["mcp:fetcher:*".to_owned(), "http".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let resolved = catalog
        .agents()
        .resolve(&tenant.id, "assistant")
        .await
        .unwrap();
    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.capabilities.len(), 2);
    assert_eq!(resolved.memory_pool.unwrap().id.as_str(), pool.id.as_str());
}

// This catalog uses a single `Arc<Mutex<Connection>>` by design for
// Increment 1, so concurrent writers cannot interleave with reads at the SQL
// level. This test verifies that `resolve` returns an internally consistent
// snapshot (no torn reads), which is satisfied by either: (a) wrapping joins
// in a transaction (current impl), or (b) the mutex serialization itself.
// When the catalog moves to a multi-connection pool, this test should be
// hardened to inject a writer between resolve's join queries on a separate
// connection — at which point the in-transaction guarantee becomes
// load-bearing.
#[tokio::test]
async fn resolve_returns_consistent_snapshot_under_concurrent_writer() {
    use std::sync::Arc as StdArc;
    use tokio::sync::Barrier;

    let catalog = StdArc::new(fresh());
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill_alpha = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "alpha-body",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let skill_beta = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "beta",
                description: None,
                body: "beta-body",
                metadata: None,
            },
        )
        .await
        .unwrap();

    let initial_skill_ids = vec![skill_alpha.id.clone(), skill_beta.id.clone()];
    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: Some(10),
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: &initial_skill_ids,
                capabilities: &["mcp:fetcher:*".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let barrier = StdArc::new(Barrier::new(2));

    // Reader: resolve the agent
    let reader_catalog = StdArc::clone(&catalog);
    let reader_tenant = tenant.id.clone();
    let reader_barrier = StdArc::clone(&barrier);
    let reader = tokio::spawn(async move {
        reader_barrier.wait().await;
        reader_catalog
            .agents()
            .resolve(&reader_tenant, "assistant")
            .await
    });

    // Writer: delete one of the skills concurrently
    let writer_catalog = StdArc::clone(&catalog);
    let writer_tenant = tenant.id.clone();
    let writer_barrier = StdArc::clone(&barrier);
    let writer_skill_id = skill_beta.id.clone();
    let writer = tokio::spawn(async move {
        writer_barrier.wait().await;
        writer_catalog
            .skills()
            .delete(&writer_tenant, &writer_skill_id)
            .await
    });

    let (resolve_res, delete_res) = tokio::join!(reader, writer);
    let resolved = resolve_res.unwrap().unwrap();
    delete_res.unwrap().unwrap();

    // Single-transaction snapshot semantics: the resolved skills set must
    // either contain both skills (resolve ran before the delete committed)
    // or contain only alpha (resolve ran after delete committed).
    // It must NEVER contain a torn-write state where an agent_skills row
    // points to a skill_id that has no matching skills row.
    let resolved_ids: Vec<String> = resolved
        .skills
        .iter()
        .map(|s| s.id.as_str().to_owned())
        .collect();
    let only_alpha = resolved_ids == vec![skill_alpha.id.as_str().to_owned()];
    let both = resolved_ids.len() == 2
        && resolved_ids.contains(&skill_alpha.id.as_str().to_owned())
        && resolved_ids.contains(&skill_beta.id.as_str().to_owned());
    assert!(
        only_alpha || both,
        "resolved skills must be a consistent snapshot, got: {:?}",
        resolved_ids
    );

    // All resolved skills must have a complete body — no torn writes.
    for s in &resolved.skills {
        assert!(
            !s.body.is_empty(),
            "torn-write: skill {} has empty body",
            s.id
        );
    }
}

#[tokio::test]
async fn resolve_returns_not_found_for_missing_agent() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();

    let err = catalog
        .agents()
        .resolve(&tenant.id, "missing")
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

// ----- Some(None) patch tests for nullable fields (BLOCKER 2) -----

#[tokio::test]
async fn agent_update_description_set_to_null() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: Some("original"),
                system_prompt: "prompt",
                model: "model",
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

    let updated = catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                description: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.description, None);

    // Re-fetch to confirm persistence.
    let refetched = catalog.agents().get(&tenant.id, &agent.id).await.unwrap();
    assert_eq!(refetched.description, None);
}

#[tokio::test]
async fn agent_update_max_tokens_set_to_null() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: Some(2048),
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let updated = catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                max_tokens: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.max_tokens, None);

    let refetched = catalog.agents().get(&tenant.id, &agent.id).await.unwrap();
    assert_eq!(refetched.max_tokens, None);
}

#[tokio::test]
async fn agent_update_memory_pool_id_set_to_null() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let pool = catalog
        .memory_pools()
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: Some(&pool.id),
                skill_ids: &[],
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let updated = catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                memory_pool_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.memory_pool_id, None);

    let refetched = catalog.agents().get(&tenant.id, &agent.id).await.unwrap();
    assert_eq!(refetched.memory_pool_id, None);
}

#[tokio::test]
async fn skill_update_description_set_to_null() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: Some("original"),
                body: "body",
                metadata: None,
            },
        )
        .await
        .unwrap();

    let updated = catalog
        .skills()
        .update(
            &tenant.id,
            &skill.id,
            SkillPatch {
                description: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.description, None);

    let refetched = catalog.skills().get(&tenant.id, &skill.id).await.unwrap();
    assert_eq!(refetched.description, None);
}

#[tokio::test]
async fn skill_update_metadata_set_to_null() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "body",
                metadata: Some(&json!({"x": 1})),
            },
        )
        .await
        .unwrap();

    let updated = catalog
        .skills()
        .update(
            &tenant.id,
            &skill.id,
            SkillPatch {
                metadata: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.metadata, None);

    let refetched = catalog.skills().get(&tenant.id, &skill.id).await.unwrap();
    assert_eq!(refetched.metadata, None);
}

#[tokio::test]
async fn memory_pool_update_embedding_model_set_to_null() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let pool = catalog
        .memory_pools()
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: Some("local-st-mini"),
                config: &json!({}),
            },
        )
        .await
        .unwrap();

    let updated = catalog
        .memory_pools()
        .update(
            &tenant.id,
            &pool.id,
            MemoryPoolPatch {
                embedding_model: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.embedding_model, None);

    let refetched = catalog
        .memory_pools()
        .get(&tenant.id, &pool.id)
        .await
        .unwrap();
    assert_eq!(refetched.embedding_model, None);
}

// ----- Sibling no-change-semantics tests for nullable patches -----

#[tokio::test]
async fn skill_update_with_none_outer_preserves_description() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: Some("kept"),
                body: "body",
                metadata: Some(&json!({"x": 1})),
            },
        )
        .await
        .unwrap();

    let updated = catalog
        .skills()
        .update(
            &tenant.id,
            &skill.id,
            SkillPatch {
                body: Some("new-body"),
                description: None,
                metadata: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.description.as_deref(), Some("kept"));
    assert_eq!(updated.metadata, Some(json!({"x": 1})));
    assert_eq!(updated.body, "new-body");
}

#[tokio::test]
async fn memory_pool_update_with_none_outer_preserves_embedding_model() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let pool = catalog
        .memory_pools()
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: Some("kept-model"),
                config: &json!({"vector_dim": 384}),
            },
        )
        .await
        .unwrap();

    let updated = catalog
        .memory_pools()
        .update(
            &tenant.id,
            &pool.id,
            MemoryPoolPatch {
                name: Some("renamed"),
                embedding_model: None,
                config: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "renamed");
    assert_eq!(updated.embedding_model.as_deref(), Some("kept-model"));
    assert_eq!(updated.config, json!({"vector_dim": 384}));
}

// ----- Tenant cascade test (WARNING 4) -----

#[tokio::test]
async fn tenant_delete_cascades_agents_skills_memory_pools() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let pool = catalog
        .memory_pools()
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap();
    let skill = catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name: "alpha",
                description: None,
                body: "body",
                metadata: None,
            },
        )
        .await
        .unwrap();
    let _agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: Some(&pool.id),
                skill_ids: std::slice::from_ref(&skill.id),
                capabilities: &["mcp:fetcher:*".to_owned()],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();

    let tenant_id_str = tenant.id.as_str().to_owned();

    // Delete the tenant via raw SQL (TenantRepository doesn't expose delete in this scope).
    {
        let conn = catalog.conn_for_tests();
        let guard = conn.lock().unwrap();
        guard
            .execute(
                "DELETE FROM tenants WHERE id = ?1",
                [tenant_id_str.as_str()],
            )
            .unwrap();
    }

    // Each child table should now have zero rows.
    let conn = catalog.conn_for_tests();
    let guard = conn.lock().unwrap();
    for table in [
        "agents",
        "skills",
        "memory_pools",
        "agent_skills",
        "agent_capabilities",
    ] {
        let count: i64 = guard
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 0,
            "expected zero rows in {table} after tenant cascade delete"
        );
    }
}

// ----- Agent's memory_pool_id set NULL on pool delete (NIT 4) -----

#[tokio::test]
async fn agent_memory_pool_id_set_null_on_pool_delete() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let pool = catalog
        .memory_pools()
        .create(
            &tenant.id,
            NewMemoryPool {
                name: "shared",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap();
    let agent = catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: Some(&pool.id),
                skill_ids: &[],
                capabilities: &[],
                channel_ids: &[],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        agent
            .memory_pool_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        Some(pool.id.as_str().to_owned())
    );

    catalog
        .memory_pools()
        .delete(&tenant.id, &pool.id)
        .await
        .unwrap();

    let refetched = catalog.agents().get(&tenant.id, &agent.id).await.unwrap();
    assert_eq!(refetched.memory_pool_id, None);
}

// ----- name_contains SQL filter combined with pagination (Phase 4 fix) -----

/// Seeds 5 agents whose names produce a precise 3-of-5 match for the needle
/// "matchme": three contain the substring (matchme-one, matchme-two,
/// matchme-three), two do not (other-one, other-two). Verifies that
/// `name_contains = Some("matchme")` is applied at the SQL layer so pageInfo
/// reflects the *filtered* universe — not the broken row-then-filter path
/// where `has_next_page` could be `true` on a page that yielded zero matches.
#[tokio::test]
async fn agent_list_sql_name_contains_filter_combined_with_pagination() {
    let catalog = fresh();
    let tenant = catalog.tenants().create("acme", None).await.unwrap();
    let agents = catalog.agents();

    // Insertion order is preserved by created_at ordering in the SQL repo.
    let insertion_order = [
        "matchme-one",
        "other-one",
        "matchme-two",
        "other-two",
        "matchme-three",
    ];
    for name in insertion_order {
        agents
            .create(
                &tenant.id,
                NewAgent {
                    name,
                    description: None,
                    system_prompt: "p",
                    model: "m",
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

    // First page: filter "matchme" with first=2 → exactly 2 matches, more available.
    let page1 = agents
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: None,
                last: None,
                before: None,
            },
            Some("matchme"),
        )
        .await
        .unwrap();
    assert_eq!(page1.items.len(), 2);
    assert!(
        page1.has_next_page,
        "page1 must report has_next_page when 3 of 5 rows match"
    );
    let cursor1 = page1.end_cursor.clone().expect("page1 needs end_cursor");
    let page1_names: Vec<String> = page1.items.iter().map(|a| a.name.clone()).collect();
    assert_eq!(
        page1_names,
        vec!["matchme-one".to_owned(), "matchme-two".to_owned()]
    );

    // Second page with cursor → exactly 1 match (matchme-three), no more.
    let page2 = agents
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: Some(cursor1),
                last: None,
                before: None,
            },
            Some("matchme"),
        )
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 1);
    assert!(
        !page2.has_next_page,
        "page2 must terminate when only 3 rows match the filter overall"
    );
    assert_eq!(page2.items[0].name, "matchme-three");

    // Sanity: a needle that matches nothing yields a clean empty page with
    // has_next_page=false (this is the precise pageInfo bug the SQL push fixes).
    let empty = agents
        .list(
            &tenant.id,
            PageRequest::default(),
            Some("zzzz-does-not-match"),
        )
        .await
        .unwrap();
    assert!(empty.items.is_empty());
    assert!(!empty.has_next_page);
}
