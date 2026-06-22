use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use simulacra_catalog::models::{
    Agent, MemoryPool, NewAgent, NewMemoryPool, PageRequest, Skill, Tenant,
};
use simulacra_catalog::repo::memory::{
    InMemoryFixtures, MemoryAgentRepository, MemoryMemoryPoolRepository, MemorySkillRepository,
    MemoryTenantRepository,
};
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{AgentId, CatalogError, MemoryPoolId, SkillId, TenantId};

fn fixture_tenant(namespace: &str) -> Tenant {
    let now = Utc::now();
    Tenant {
        id: TenantId::from(format!("{namespace}-tenant")),
        namespace: namespace.to_owned(),
        display_name: Some(namespace.to_owned()),
        created_at: now,
        updated_at: now,
    }
}

fn fixture_skill(tenant_id: &TenantId, name: &str) -> Skill {
    let now = Utc::now();
    Skill {
        id: SkillId::new(),
        tenant_id: tenant_id.clone(),
        name: name.to_owned(),
        description: Some(format!("{name} description")),
        body: format!("{name} body"),
        metadata: Some(json!({"name": name})),
        created_at: now,
        updated_at: now,
    }
}

fn fixture_agent(tenant_id: &TenantId, name: &str) -> Agent {
    let now = Utc::now();
    Agent {
        id: AgentId::new(),
        tenant_id: tenant_id.clone(),
        name: name.to_owned(),
        description: Some(format!("{name} description")),
        system_prompt: format!("{name} prompt"),
        model: "openai/gpt-oss-120b".to_owned(),
        max_turns: 32,
        max_tokens: Some(2048),
        memory_pool_id: None,
        created_at: now,
        updated_at: now,
    }
}

fn fixture_memory_pool(tenant_id: &TenantId, name: &str) -> MemoryPool {
    let now = Utc::now();
    MemoryPool {
        id: MemoryPoolId::new(),
        tenant_id: tenant_id.clone(),
        name: name.to_owned(),
        embedding_model: Some("local-st-mini".to_owned()),
        config: json!({"vector_dim": 384}),
        created_at: now,
        updated_at: now,
    }
}

/// Fixtures with memory pools — used by memory_pool tests.
fn fixtures_with_pools() -> Arc<InMemoryFixtures> {
    let acme = fixture_tenant("acme");
    let other = fixture_tenant("other");
    let skill = fixture_skill(&acme.id, "alpha");
    let agent = fixture_agent(&acme.id, "assistant");
    let acme_pool = fixture_memory_pool(&acme.id, "shared");

    Arc::new(InMemoryFixtures {
        tenants: [
            (acme.id.clone(), acme.clone()),
            (other.id.clone(), other.clone()),
        ]
        .into_iter()
        .collect(),
        agents: [(agent.id.clone(), agent.clone())].into_iter().collect(),
        agent_skills: [(agent.id.clone(), vec![skill.id.clone()])]
            .into_iter()
            .collect(),
        agent_capabilities: [(agent.id.clone(), vec!["mcp:fetcher:*".to_owned()])]
            .into_iter()
            .collect(),
        skills: [(skill.id.clone(), skill)].into_iter().collect(),
        memory_pools: [(acme_pool.id.clone(), acme_pool)].into_iter().collect(),
        agent_files: Default::default(),
        channels: Default::default(),
        agent_channels: Default::default(),
    })
}

fn fixtures() -> Arc<InMemoryFixtures> {
    let acme = fixture_tenant("acme");
    let other = fixture_tenant("other");
    let skill = fixture_skill(&acme.id, "alpha");
    let agent = fixture_agent(&acme.id, "assistant");

    Arc::new(InMemoryFixtures {
        tenants: [(acme.id.clone(), acme.clone()), (other.id.clone(), other)]
            .into_iter()
            .collect(),
        agents: [(agent.id.clone(), agent.clone())].into_iter().collect(),
        agent_skills: [(agent.id.clone(), vec![skill.id.clone()])]
            .into_iter()
            .collect(),
        agent_capabilities: [(agent.id.clone(), vec!["mcp:fetcher:*".to_owned()])]
            .into_iter()
            .collect(),
        skills: [(skill.id.clone(), skill)].into_iter().collect(),
        memory_pools: Default::default(),
        agent_files: Default::default(),
        channels: Default::default(),
        agent_channels: Default::default(),
    })
}

#[tokio::test]
async fn resolve_serves_in_memory_agent() {
    let fixtures = fixtures();
    // Capture the fixture-known agent + skill for independent expectation-building.
    let agent_id = fixtures.agents.keys().next().unwrap().clone();
    let skill_id = fixtures.agent_skills.values().next().unwrap()[0].clone();
    let expected_skill = fixtures.skills.get(&skill_id).cloned().unwrap();

    let repo = MemoryAgentRepository::new(Arc::clone(&fixtures));

    let resolved = repo
        .resolve(&TenantId::from("acme-tenant"), "assistant")
        .await
        .unwrap();

    // Build the expected ResolvedAgent independently from the fixture inputs.
    // Every field must match.
    assert_eq!(resolved.id, agent_id, "id mismatch");
    assert_eq!(resolved.name, "assistant", "name mismatch");
    assert_eq!(
        resolved.system_prompt, "assistant prompt",
        "system_prompt mismatch"
    );
    assert_eq!(resolved.model, "openai/gpt-oss-120b", "model mismatch");
    assert_eq!(resolved.max_turns, 32, "max_turns mismatch");
    assert_eq!(resolved.max_tokens, Some(2048), "max_tokens mismatch");

    // Skills: length AND each skill's name + body.
    assert_eq!(resolved.skills.len(), 1, "skills len mismatch");
    assert_eq!(
        resolved.skills[0].id, expected_skill.id,
        "skill id mismatch"
    );
    assert_eq!(
        resolved.skills[0].name, expected_skill.name,
        "skill name mismatch"
    );
    assert_eq!(
        resolved.skills[0].body, expected_skill.body,
        "skill body mismatch"
    );

    // Capabilities.
    assert_eq!(
        resolved.capabilities,
        vec!["mcp:fetcher:*".to_owned()],
        "capabilities mismatch"
    );

    // Memory pool: this fixture has no memory pool, so it must be None.
    assert!(resolved.memory_pool.is_none(), "memory_pool should be None");
}

#[tokio::test]
async fn get_returns_not_found_for_unknown_id() {
    let fixtures = fixtures();
    let agents = MemoryAgentRepository::new(Arc::clone(&fixtures));
    let skills = MemorySkillRepository::new(fixtures);

    let agent_err = agents
        .get(
            &TenantId::from("acme-tenant"),
            &AgentId::from("missing-agent"),
        )
        .await
        .unwrap_err();
    assert!(matches!(agent_err, CatalogError::NotFound(_)));

    let skill_err = skills
        .get(
            &TenantId::from("acme-tenant"),
            &SkillId::from("missing-skill"),
        )
        .await
        .unwrap_err();
    assert!(matches!(skill_err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn get_serves_in_memory_agent() {
    let fixtures = fixtures();
    let agent_id = fixtures.agents.keys().next().unwrap().clone();
    let repo = MemoryAgentRepository::new(fixtures);

    let agent = repo
        .get(&TenantId::from("acme-tenant"), &agent_id)
        .await
        .unwrap();

    assert_eq!(agent.name, "assistant");
}

#[tokio::test]
async fn cross_tenant_resolve_returns_not_found() {
    let fixtures = fixtures();
    let repo = MemoryAgentRepository::new(fixtures);

    let err = repo
        .resolve(&TenantId::from("other-tenant"), "assistant")
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn create_returns_readonly_error() {
    let fixtures = fixtures();
    let agents = MemoryAgentRepository::new(Arc::clone(&fixtures));
    let skills = MemorySkillRepository::new(fixtures);

    let agent_err = agents
        .create(
            &TenantId::from("acme-tenant"),
            NewAgent {
                name: "new-agent",
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
    assert!(matches!(agent_err, CatalogError::ReadOnly(_)));

    let skill_err = skills
        .create(
            &TenantId::from("acme-tenant"),
            simulacra_catalog::NewSkill {
                name: "new-skill",
                description: None,
                body: "body",
                metadata: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(skill_err, CatalogError::ReadOnly(_)));
}

#[tokio::test]
async fn update_returns_readonly_error() {
    let fixtures = fixtures();
    let agents = MemoryAgentRepository::new(Arc::clone(&fixtures));
    let skills = MemorySkillRepository::new(fixtures);

    let agent_err = agents
        .update(
            &TenantId::from("acme-tenant"),
            &AgentId::from("agent"),
            simulacra_catalog::AgentPatch::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(agent_err, CatalogError::ReadOnly(_)));

    let skill_err = skills
        .update(
            &TenantId::from("acme-tenant"),
            &SkillId::from("skill"),
            simulacra_catalog::SkillPatch::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(skill_err, CatalogError::ReadOnly(_)));
}

#[tokio::test]
async fn delete_returns_readonly_error() {
    let fixtures = fixtures();
    let agents = MemoryAgentRepository::new(Arc::clone(&fixtures));
    let skills = MemorySkillRepository::new(fixtures);

    let agent_err = agents
        .delete(&TenantId::from("acme-tenant"), &AgentId::from("agent"))
        .await
        .unwrap_err();
    assert!(matches!(agent_err, CatalogError::ReadOnly(_)));

    let skill_err = skills
        .delete(&TenantId::from("acme-tenant"), &SkillId::from("skill"))
        .await
        .unwrap_err();
    assert!(matches!(skill_err, CatalogError::ReadOnly(_)));
}

#[tokio::test]
async fn list_serves_in_memory_agents() {
    let fixtures = fixtures();
    let repo = MemoryAgentRepository::new(fixtures);

    let page = repo
        .list(&TenantId::from("acme-tenant"), PageRequest::default(), None)
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].name, "assistant");
}

#[tokio::test]
async fn cross_tenant_get_returns_not_found() {
    let fixtures = fixtures();
    let agent_id = fixtures.agents.keys().next().unwrap().clone();
    let repo = MemoryAgentRepository::new(fixtures);

    let err = repo
        .get(&TenantId::from("other-tenant"), &agent_id)
        .await
        .unwrap_err();

    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn cross_tenant_list_returns_empty_page() {
    let fixtures = fixtures();
    let repo = MemoryAgentRepository::new(fixtures);

    let page = repo
        .list(
            &TenantId::from("other-tenant"),
            PageRequest::default(),
            None,
        )
        .await
        .unwrap();

    assert!(page.items.is_empty());
}

#[tokio::test]
async fn capabilities_returns_visible_agent_capabilities() {
    let fixtures = fixtures();
    let agent_id = fixtures.agents.keys().next().unwrap().clone();
    let repo = MemoryAgentRepository::new(fixtures);

    let capabilities = repo
        .capabilities(&TenantId::from("acme-tenant"), &agent_id)
        .await
        .unwrap();

    assert_eq!(capabilities, vec!["mcp:fetcher:*".to_owned()]);
}

#[tokio::test]
async fn capabilities_hide_cross_tenant_agent() {
    let fixtures = fixtures();
    let agent_id = fixtures.agents.keys().next().unwrap().clone();
    let repo = MemoryAgentRepository::new(fixtures);

    let capabilities = repo
        .capabilities(&TenantId::from("other-tenant"), &agent_id)
        .await
        .unwrap();

    assert!(capabilities.is_empty());
}

// ----- In-memory TenantRepository coverage (WARNING 1) -----

#[tokio::test]
async fn tenant_get_by_namespace_serves_fixture() {
    let fixtures = fixtures();
    let repo = MemoryTenantRepository::new(Arc::clone(&fixtures));

    let tenant = repo.get_by_namespace("acme").await.unwrap();
    assert_eq!(tenant.namespace, "acme");
    assert_eq!(tenant.id, TenantId::from("acme-tenant"));
}

#[tokio::test]
async fn tenant_get_by_id_serves_fixture() {
    let fixtures = fixtures();
    let repo = MemoryTenantRepository::new(Arc::clone(&fixtures));

    let tenant = repo
        .get_by_id(&TenantId::from("acme-tenant"))
        .await
        .unwrap();
    assert_eq!(tenant.namespace, "acme");
}

#[tokio::test]
async fn tenant_get_by_unknown_namespace_returns_not_found() {
    let fixtures = fixtures();
    let repo = MemoryTenantRepository::new(fixtures);

    let err = repo.get_by_namespace("nonexistent").await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn tenant_create_returns_readonly_error() {
    let fixtures = fixtures();
    let repo = MemoryTenantRepository::new(fixtures);

    let err = repo
        .create("new-namespace", Some("Display"))
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::ReadOnly(_)));
}

// ----- In-memory MemoryPoolRepository coverage (WARNING 1) -----

#[tokio::test]
async fn memory_pool_get_serves_fixture() {
    let fixtures = fixtures_with_pools();
    let pool_id = fixtures.memory_pools.keys().next().unwrap().clone();
    let repo = MemoryMemoryPoolRepository::new(Arc::clone(&fixtures));

    let pool = repo
        .get(&TenantId::from("acme-tenant"), &pool_id)
        .await
        .unwrap();
    assert_eq!(pool.name, "shared");
    assert_eq!(pool.tenant_id, TenantId::from("acme-tenant"));
}

#[tokio::test]
async fn memory_pool_get_unknown_returns_not_found() {
    let fixtures = fixtures_with_pools();
    let repo = MemoryMemoryPoolRepository::new(fixtures);

    let err = repo
        .get(&TenantId::from("acme-tenant"), &MemoryPoolId::new())
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn memory_pool_create_returns_readonly_error() {
    let fixtures = fixtures_with_pools();
    let repo = MemoryMemoryPoolRepository::new(fixtures);

    let err = repo
        .create(
            &TenantId::from("acme-tenant"),
            NewMemoryPool {
                name: "new-pool",
                embedding_model: None,
                config: &json!({}),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::ReadOnly(_)));
}

#[tokio::test]
async fn memory_pool_cross_tenant_get_returns_not_found() {
    let fixtures = fixtures_with_pools();
    let pool_id = fixtures.memory_pools.keys().next().unwrap().clone();
    let repo = MemoryMemoryPoolRepository::new(fixtures);

    // The pool belongs to "acme-tenant"; querying with "other-tenant" must fail.
    let err = repo
        .get(&TenantId::from("other-tenant"), &pool_id)
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

// ----- B2: cross-tenant join filtering in resolve() -----

/// Construct a fixture set where an `agent_skills` row references a Skill that
/// belongs to a different tenant than the agent (a miswired fixture). The
/// `resolve` path must filter such joined rows so it never surfaces foreign-
/// tenant data — even if the join table itself happens to point at it.
#[tokio::test]
async fn memory_resolve_skips_cross_tenant_joined_skills() {
    let acme = fixture_tenant("acme");
    let other = fixture_tenant("other");
    let agent = fixture_agent(&acme.id, "assistant");
    let acme_skill = fixture_skill(&acme.id, "alpha");
    // This skill belongs to `other` but is dangling-joined to acme's agent.
    let foreign_skill = fixture_skill(&other.id, "beta");
    // A pool that ALSO belongs to `other` but is referenced by acme's agent.
    let foreign_pool = fixture_memory_pool(&other.id, "foreign-pool");
    let mut acme_agent_with_pool = agent.clone();
    acme_agent_with_pool.memory_pool_id = Some(foreign_pool.id.clone());

    let fixtures = Arc::new(InMemoryFixtures {
        tenants: [(acme.id.clone(), acme.clone()), (other.id.clone(), other)]
            .into_iter()
            .collect(),
        agents: [(
            acme_agent_with_pool.id.clone(),
            acme_agent_with_pool.clone(),
        )]
        .into_iter()
        .collect(),
        agent_skills: [(
            acme_agent_with_pool.id.clone(),
            vec![acme_skill.id.clone(), foreign_skill.id.clone()],
        )]
        .into_iter()
        .collect(),
        agent_capabilities: Default::default(),
        skills: [
            (acme_skill.id.clone(), acme_skill.clone()),
            (foreign_skill.id.clone(), foreign_skill.clone()),
        ]
        .into_iter()
        .collect(),
        memory_pools: [(foreign_pool.id.clone(), foreign_pool)]
            .into_iter()
            .collect(),
        agent_files: Default::default(),
        channels: Default::default(),
        agent_channels: Default::default(),
    });

    let repo = MemoryAgentRepository::new(fixtures);
    let resolved = repo
        .resolve(&TenantId::from("acme-tenant"), "assistant")
        .await
        .unwrap();

    // Only the acme-tenant skill must survive the join.
    assert_eq!(resolved.skills.len(), 1, "cross-tenant skill leaked");
    assert_eq!(resolved.skills[0].id, acme_skill.id);
    assert_eq!(
        resolved.skills[0].tenant_id,
        TenantId::from("acme-tenant"),
        "leaked skill belonged to a foreign tenant"
    );

    // The foreign memory pool must be filtered out as well.
    assert!(
        resolved.memory_pool.is_none(),
        "cross-tenant memory_pool leaked into resolved agent"
    );
}

// ----- W1: in-memory list pagination -----

/// Insert 5 agents with monotonically advancing `created_at` and walk the
/// pages with `first: 2`, then `first: 2, after: <end_cursor>`, then
/// `first: 2, after: <next end_cursor>`. All 5 agents must be returned across
/// the three pages with no overlap and no duplicates.
#[tokio::test]
async fn memory_list_paginates_with_first_and_after() {
    use chrono::TimeZone;

    let acme = fixture_tenant("acme");
    let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

    // Five agents at strictly increasing created_at timestamps.
    let mut agents_map = std::collections::HashMap::new();
    let mut expected_ids: Vec<String> = Vec::new();
    for i in 0..5 {
        let mut a = fixture_agent(&acme.id, &format!("agent-{i}"));
        a.created_at = base + chrono::Duration::seconds(i as i64);
        a.updated_at = a.created_at;
        expected_ids.push(a.id.as_str().to_owned());
        agents_map.insert(a.id.clone(), a);
    }

    let fixtures = Arc::new(InMemoryFixtures {
        tenants: [(acme.id.clone(), acme.clone())].into_iter().collect(),
        agents: agents_map,
        agent_skills: Default::default(),
        agent_capabilities: Default::default(),
        skills: Default::default(),
        memory_pools: Default::default(),
        agent_files: Default::default(),
        channels: Default::default(),
        agent_channels: Default::default(),
    });
    let repo = MemoryAgentRepository::new(fixtures);
    let tenant = TenantId::from("acme-tenant");

    // Page 1 — first: 2, no cursor.
    let page1 = repo
        .list(
            &tenant,
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
    assert_eq!(page1.items.len(), 2, "page1 size");
    assert!(page1.has_next_page, "page1 should report more");
    let cursor1 = page1.end_cursor.clone().expect("page1 needs end_cursor");

    // Page 2 — first: 2, after = page1.end_cursor.
    let page2 = repo
        .list(
            &tenant,
            PageRequest {
                first: Some(2),
                after: Some(cursor1),
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(page2.items.len(), 2, "page2 size");
    assert!(page2.has_next_page, "page2 should report more");
    let cursor2 = page2.end_cursor.clone().expect("page2 needs end_cursor");

    // Page 3 — first: 2, after = page2.end_cursor. Should yield the last 1.
    let page3 = repo
        .list(
            &tenant,
            PageRequest {
                first: Some(2),
                after: Some(cursor2),
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(page3.items.len(), 1, "page3 size");
    assert!(!page3.has_next_page, "page3 should be terminal");

    // Stitch the pages together — must equal the expected ordering with no
    // dupes and no gaps.
    let mut got: Vec<String> = Vec::new();
    got.extend(page1.items.iter().map(|a| a.id.as_str().to_owned()));
    got.extend(page2.items.iter().map(|a| a.id.as_str().to_owned()));
    got.extend(page3.items.iter().map(|a| a.id.as_str().to_owned()));
    assert_eq!(
        got, expected_ids,
        "paginated results must match created_at-ordered fixture set"
    );
    let mut deduped = got.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), got.len(), "duplicate row across pages");
}

// ----- W5: get_or_create alignment with SQLite -----

#[tokio::test]
async fn memory_tenant_get_or_create_unknown_returns_readonly() {
    let fixtures = fixtures();
    let repo = MemoryTenantRepository::new(fixtures);

    // Existing namespace returns the fixture row.
    let existing = repo.get_or_create("acme", Some("Display")).await.unwrap();
    assert_eq!(existing.namespace, "acme");

    // Unknown namespace must surface ReadOnly (the in-memory repo cannot
    // create), aligning with the SQLite get_or_create's create-on-miss
    // behaviour but failing safe rather than silently differing.
    let err = repo
        .get_or_create("nonexistent", Some("Display"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CatalogError::ReadOnly(_)),
        "expected ReadOnly, got {err:?}"
    );
}
