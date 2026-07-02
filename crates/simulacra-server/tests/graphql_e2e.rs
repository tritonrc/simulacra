//! S042 Inc 3 Task 13 — End-to-end seam smoke.
//!
//! Closes the seam between "agent created via the GraphQL API" and "agent
//! runs from the catalog inside `SimulacraEngine`". A single in-memory `Catalog`
//! instance backs both the GraphQL schema and the engine — the same row the
//! `createAgent` mutation writes is the row `engine.spawn_task` resolves.
//!
//! ─────────────────────────────────────────────────────────────────────────
//! Spec coverage (specs/S042-agent-catalog-graphql.md §"E2E (Phase 3a)",
//! lines 569–574):
//!
//! - line 570 ✓ `createAgent` mutation creates a row that is observable on
//!   the same catalog handle the engine reads.
//! - line 571 ✓ Subsequent task creation (here via `engine.spawn_task` —
//!   the runtime entry point S031's HTTP API delegates to) resolves the
//!   catalog-defined agent.
//! - line 574 ✓ A skill authored via `createSkill` appears at
//!   `/skills/<name>/SKILL.md` and `/var/skills/<name>.md` in the running
//!   task's composed VFS.
//!
//! Deferred (NOT exercised here, with rationale):
//! - line 572 ("Agent runs to completion against a recording HTTP fixture"):
//!   `SimulacraEngine::spawn_task` constructs the LLM provider in-band
//!   (`AnthropicProvider` / `OpenAiProvider`) directly from `ANTHROPIC_API_KEY`
//!   / `OPENAI_API_KEY` env vars (engine.rs:1187–1192). There is no provider
//!   injection seam today, and no recording-HTTP fixture lives in
//!   `simulacra-server/tests/`. Wiring a recording fixture (mockito or similar)
//!   plus the provider-injection seam to consume it is a larger change than
//!   this seam-smoke task — it deserves its own follow-up. Per
//!   `feedback_no_framework_only.md`, this gap is named explicitly here
//!   rather than papered over with a pseudo-test.
//!
//! Strategy: drive the GraphQL schema directly with `Schema::data()` for
//! `GraphQLContext` (mirrors `crates/simulacra-graphql/tests/queries.rs`'s
//! `schema_with_seeded_catalog`). No axum router or auth middleware — the
//! seam under test is *catalog ↔ engine*, not HTTP transport (which is
//! covered by `crates/simulacra-graphql/tests/auth.rs`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_graphql::{EmptySubscription, Schema};
use serde_json::{Value, json};
use simulacra_catalog::repo::{
    AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{Catalog, Tenant};
use simulacra_config::{CatalogConfig, ProjectConfig, SimulacraConfig, VfsConfig};
use simulacra_graphql::context::{AuthenticatedPrincipal, GraphQLContext};
use simulacra_graphql::schema::{MutationRoot, QueryRoot};
use simulacra_server::{BudgetPoolConfig, SimulacraEngine, TaskManager, TaskState, TenantConfig};
use simulacra_types::VirtualFs;

// ─── Fixtures ────────────────────────────────────────────────────────────

fn empty_config() -> SimulacraConfig {
    // INTENTIONALLY EMPTY agent_types: the spawn under test must come from
    // the catalog row created by `createAgent`, not from a fallback config
    // path. Mirror `engine_catalog.rs::catalog_backed_config`.
    SimulacraConfig {
        project: ProjectConfig {
            name: "graphql-e2e".to_string(),
            description: None,
        },
        agent_types: HashMap::new(),
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: None,
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: CatalogConfig::default(),
    }
}

fn tenant_config(namespace: &str, agent_type: &str) -> TenantConfig {
    TenantConfig {
        namespace: namespace.to_string(),
        agent_type: agent_type.to_string(),
        vfs_root: PathBuf::from(format!("/tmp/{namespace}")),
        budget_pool: BudgetPoolConfig::default(),
        hooks: vec![],
        integrations: vec![],
        mcp_servers: Default::default(),
    }
}

async fn ensure_tenant(catalog: &Catalog, namespace: &str) -> Tenant {
    catalog
        .tenants()
        .get_or_create(namespace, Some(namespace))
        .await
        .expect("tenant should exist")
}

/// Build a simulacra-graphql schema with `GraphQLContext` pre-injected so we
/// don't need to wire the auth middleware just to test the catalog↔engine
/// seam. The schema and the engine are constructed over the *same*
/// `Catalog` repository handles so a row written by a mutation is
/// immediately visible to `engine.spawn_task`.
fn build_schema_for_tenant(
    catalog: &Catalog,
    tenant: &Tenant,
) -> Schema<QueryRoot, MutationRoot, EmptySubscription> {
    Schema::build(
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
            subject: "e2e-test".to_owned(),
        },
    })
    .finish()
}

fn build_engine(catalog: &Catalog) -> SimulacraEngine {
    SimulacraEngine::new(
        empty_config(),
        None,
        Arc::new(catalog.agents()) as Arc<dyn AgentRepository>,
        Arc::new(catalog.skills()) as Arc<dyn SkillRepository>,
        Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>,
        Arc::new(catalog.tenants()) as Arc<dyn TenantRepository>,
    )
    .expect("engine should construct over shared catalog handles")
}

/// Run a GraphQL operation and return the parsed JSON `data` payload, or
/// panic with the GraphQL error list. Inline because the test suite never
/// expects a *valid* operation to surface errors — failures should fail the
/// test fast with the precise GraphQL message.
async fn execute_or_panic(
    schema: &Schema<QueryRoot, MutationRoot, EmptySubscription>,
    op: &str,
) -> Value {
    let response = schema.execute(op).await;
    assert!(
        response.errors.is_empty(),
        "GraphQL op should succeed; errors: {:?}\nop: {op}",
        response.errors
    );
    response.data.into_json().expect("data should be JSON")
}

// ─── Tests ───────────────────────────────────────────────────────────────

/// Spec line 570 + 571: the row a `createAgent` mutation writes is
/// resolvable from the same catalog handle by `engine.spawn_task`, and the
/// engine's per-task snapshot reflects exactly the GraphQL-supplied fields.
#[tokio::test]
async fn create_agent_via_graphql_then_spawn_task_resolves_the_catalog_row() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    let schema = build_schema_for_tenant(&catalog, &tenant);

    let create_agent = r#"
        mutation {
            createAgent(input: {
                name: "e2e-runner",
                description: "created via GraphQL",
                systemPrompt: "Reply with done.",
                model: "ollama:llama3",
                maxTurns: 4,
                maxTokens: 2048,
                skillIds: [],
                capabilities: ["net:read"]
            }) {
                id
                name
                model
                systemPrompt
                maxTurns
                maxTokens
                capabilities
            }
        }
    "#;
    let data = execute_or_panic(&schema, create_agent).await;
    let agent_payload = &data["createAgent"];
    assert_eq!(agent_payload["name"], "e2e-runner");
    assert_eq!(agent_payload["model"], "ollama:llama3");
    assert_eq!(agent_payload["systemPrompt"], "Reply with done.");
    assert_eq!(agent_payload["maxTurns"], 4);
    assert_eq!(agent_payload["maxTokens"], 2048);
    let agent_id_str = agent_payload["id"]
        .as_str()
        .expect("agent id is a string")
        .to_owned();
    assert!(!agent_id_str.is_empty());

    // Same Catalog handle the engine will read — proves the seam doesn't
    // depend on transport-side caching, only on the shared repo handles.
    let engine = build_engine(&catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "use the GraphQL-authored agent",
            &tenant_config("default", "e2e-runner"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should resolve the agent the GraphQL mutation just wrote");

    // The resolution succeeded — the engine's TaskManager parked the task
    // in Pending (catalog-only happy path; nothing in this test runs the
    // agent loop, see deferred-line-572 note in module doc).
    assert_eq!(handle.state, TaskState::Pending);
    assert_eq!(handle.agent_type, "e2e-runner");

    // The *snapshot* the engine took at spawn time must mirror the row the
    // GraphQL mutation wrote. Asserting on debug_resolved_agent (rather
    // than re-querying the catalog) proves the engine carried the row into
    // the running task's state, not just that the catalog has it.
    let resolved = engine
        .debug_resolved_agent(&handle.task_id)
        .expect("engine must record a per-task snapshot for the catalog agent");
    assert_eq!(resolved.name, "e2e-runner");
    assert_eq!(resolved.system_prompt, "Reply with done.");
    assert_eq!(resolved.model, "ollama:llama3");
    assert_eq!(resolved.max_turns, 4);
    assert_eq!(resolved.max_tokens, Some(2048));
    assert_eq!(resolved.capabilities, vec!["net:read".to_string()]);
    // ID round-trip: the row the engine resolved is the row GraphQL returned.
    assert_eq!(resolved.id.as_str(), agent_id_str);
}

/// Spec line 574: a skill authored through `createSkill` is mounted at
/// `/skills/<name>/SKILL.md` and `/var/skills/<name>.md` in the running
/// task's composed VFS, with the body the mutation supplied. Proves the
/// engine reads through the same catalog the GraphQL surface writes to.
#[tokio::test]
async fn skill_authored_via_graphql_visible_at_var_skills_in_running_task() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    let schema = build_schema_for_tenant(&catalog, &tenant);

    // Create skill first; capture its id for the createAgent payload.
    let create_skill = r#"
        mutation {
            createSkill(input: {
                name: "noop",
                description: "say hi",
                body: "Just say hi when you start.",
                metadata: null
            }) {
                id
                name
            }
        }
    "#;
    let skill_data = execute_or_panic(&schema, create_skill).await;
    assert_eq!(skill_data["createSkill"]["name"], "noop");
    let skill_id = skill_data["createSkill"]["id"]
        .as_str()
        .expect("skill id is a string")
        .to_owned();

    // Now createAgent referencing the GraphQL-authored skill id. This
    // exercises the `skill_ids` validation path on createAgent (the catalog
    // join that maps id → skill body for the per-task skill VFS end-to-end through
    // the API.
    let create_agent = format!(
        r#"
        mutation {{
            createAgent(input: {{
                name: "skill-runner",
                systemPrompt: "Use the noop skill.",
                model: "ollama:llama3",
                skillIds: ["{skill_id}"],
                capabilities: []
            }}) {{
                id
                name
                skills {{ id name body }}
            }}
        }}
    "#
    );
    let agent_data = execute_or_panic(&schema, &create_agent).await;
    let skills_array = agent_data["createAgent"]["skills"]
        .as_array()
        .expect("skills should be an array");
    assert_eq!(skills_array.len(), 1, "agent should have exactly one skill");
    assert_eq!(skills_array[0]["name"], "noop");
    assert_eq!(skills_array[0]["body"], "Just say hi when you start.");

    // Spawn the task — the per-task VFS is composed at this point, so
    // /skills/noop/SKILL.md and /var/skills/noop.md must be readable.
    let engine = build_engine(&catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "exercise the skill mount",
            &tenant_config("default", "skill-runner"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task with GraphQL-authored skill should succeed");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose composed per-task VFS");

    // Canonical S017 listing must contain the skill directory and SKILL.md.
    let mut skill_listing = vfs
        .list_dir("/skills")
        .expect("/skills must expose GraphQL-authored catalog skills");
    skill_listing.sort();
    assert_eq!(skill_listing, vec!["noop"]);
    let noop_listing = vfs
        .list_dir("/skills/noop")
        .expect("/skills/noop must be a skill directory");
    assert_eq!(noop_listing, vec!["SKILL.md"]);

    // Compatibility listing must contain noop.md.
    let listing = vfs
        .list_dir("/var/skills")
        .expect("/var/skills compatibility path must be mounted over the shared catalog");
    assert!(
        listing.iter().any(|name| name == "noop.md"),
        "GraphQL-authored skill must appear at /var/skills/noop.md; got listing: {listing:?}"
    );

    // Body must round-trip with S017 frontmatter plus the catalog body.
    let canonical_rendered = vfs
        .read("/skills/noop/SKILL.md")
        .expect("/skills/noop/SKILL.md must be readable");
    let canonical_text = String::from_utf8(canonical_rendered).expect("skill file is utf8");
    assert!(
        canonical_text.contains("name: noop")
            && canonical_text.contains("description: say hi")
            && canonical_text.contains("Just say hi when you start."),
        "canonical skill must contain frontmatter and the GraphQL-supplied body, got: {canonical_text:?}"
    );

    // The compatibility path serves the same rendered document.
    let rendered = vfs
        .read("/var/skills/noop.md")
        .expect("/var/skills/noop.md must be readable");
    let rendered_text = String::from_utf8(rendered).expect("skill file is utf8");
    assert!(
        rendered_text.contains("Just say hi when you start."),
        "rendered skill must contain the GraphQL-supplied body, got: {rendered_text:?}"
    );
}

/// Defensive: a `createAgent` referencing a `skillId` from a *different*
/// tenant must fail the validation check before the row is created. Without
/// this guard the seam would let GraphQL writes leak skills across tenants
/// — even though the engine spawn path (`engine_catalog.rs`) already
/// filters cross-tenant skills server-side. The test lives here (not in
/// simulacra-graphql) because the spec assertion is about the *seam*: the
/// GraphQL surface must reject before the engine even sees it.
#[tokio::test]
async fn create_agent_rejects_skill_id_from_a_different_tenant() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let default_tenant = ensure_tenant(&catalog, "default").await;
    let other_tenant = ensure_tenant(&catalog, "other").await;

    // Create a skill in `other` directly via the repo (no GraphQL surface
    // for cross-tenant writes today). We *want* the next createAgent in
    // `default` to fail when it references this id.
    let other_skill = catalog
        .skills()
        .create(
            &other_tenant.id,
            simulacra_catalog::NewSkill {
                name: "other-skill",
                description: None,
                body: "secret",
                metadata: None,
            },
        )
        .await
        .expect("seed cross-tenant skill");

    let schema = build_schema_for_tenant(&catalog, &default_tenant);
    let create_agent = format!(
        r#"
        mutation {{
            createAgent(input: {{
                name: "leaky",
                systemPrompt: "should not be created",
                model: "ollama:llama3",
                skillIds: ["{}"],
                capabilities: []
            }}) {{ id }}
        }}
    "#,
        other_skill.id.as_str()
    );
    let response = schema.execute(create_agent.as_str()).await;
    assert!(
        !response.errors.is_empty(),
        "createAgent referencing another tenant's skill must error, but got data: {:?}",
        response.data
    );

    // Belt and braces: no agent named "leaky" should exist in default
    // tenant after the failed mutation.
    let lookup = catalog
        .agents()
        .get_by_name(&default_tenant.id, "leaky")
        .await;
    assert!(
        lookup.is_err(),
        "no row should have been written; got: {lookup:?}"
    );
}

/// Edge case: `createAgent` referencing a *nonexistent* skill id (no such
/// skill exists in any tenant) must surface a typed validation error and
/// NOT create the agent row. Closes a Phase-4 review gap that flagged
/// missing coverage for the validation branch where `skill_ids` includes
/// an id the catalog has never seen.
#[tokio::test]
async fn create_agent_rejects_nonexistent_skill_id() {
    let catalog = Catalog::open_in_memory().expect("in-memory catalog");
    let tenant = ensure_tenant(&catalog, "default").await;
    let schema = build_schema_for_tenant(&catalog, &tenant);

    // A syntactically valid ULID-shaped id that the catalog will never have.
    let phantom_skill_id = "01HZZZZZZZZZZZZZZZZZZZZZZZ";

    let create_agent = format!(
        r#"
        mutation {{
            createAgent(input: {{
                name: "ghost",
                systemPrompt: "phantom skill",
                model: "ollama:llama3",
                skillIds: ["{phantom_skill_id}"],
                capabilities: []
            }}) {{ id }}
        }}
    "#
    );
    let response = schema.execute(create_agent.as_str()).await;
    assert!(
        !response.errors.is_empty(),
        "createAgent with phantom skill id must error, but got data: {:?}",
        response.data
    );

    // The row must NOT have been written — proves validation runs before
    // INSERT, not after a failed FK rollback.
    let lookup = catalog.agents().get_by_name(&tenant.id, "ghost").await;
    assert!(
        lookup.is_err(),
        "no row should have been written when skill id is nonexistent; got: {lookup:?}"
    );
}
