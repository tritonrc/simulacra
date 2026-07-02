//! S042 Inc 3 Task 11 — `simulacra-cli` catalog bootstrap + TOML→DB import.
//!
//! These tests cover spec §"TOML import (default mode)" assertions
//! (S042 lines 554–558) plus edge cases listed in the task plan.
//!
//! Strategy: each test builds a `SimulacraConfig` literal, opens an in-memory
//! catalog, calls `import_toml_seed`, and asserts on the catalog state and
//! the returned `ImportOutcome`.

use std::collections::HashMap;
use std::path::PathBuf;

use simulacra_catalog::{Catalog, TenantId, repo::AgentRepository, repo::MemoryPoolRepository};
use simulacra_cli::catalog_import::{ImportOutcome, import_toml_seed};
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, MemoryConfig, OnModelChange, ProjectConfig,
    SimulacraConfig, VfsConfig,
};

fn base_config() -> SimulacraConfig {
    SimulacraConfig {
        project: ProjectConfig {
            name: "catalog-bootstrap-tests".into(),
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

fn agent_with(model: &str, prompt: Option<&str>) -> AgentTypeConfig {
    AgentTypeConfig {
        model: model.into(),
        system_prompt: prompt.map(str::to_owned),
        skills: vec![],
        max_turns: None,
        max_tokens: None,
        max_sub_agents: None,
        can_spawn: vec![],
        restart_policy: None,
        capabilities: None,
    }
}

async fn default_tenant_id(catalog: &Catalog) -> TenantId {
    use simulacra_catalog::repo::TenantRepository;
    catalog
        .tenants()
        .get_by_namespace("default")
        .await
        .expect("default tenant should exist after import")
        .id
}

#[tokio::test]
async fn fresh_db_imports_agent_types_from_toml() {
    let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
    let mut config = base_config();
    config
        .agent_types
        .insert("planner".into(), agent_with("claude-x", Some("plan!")));
    config
        .agent_types
        .insert("coder".into(), agent_with("claude-y", Some("code!")));

    let outcome = import_toml_seed(&catalog, &config)
        .await
        .expect("import should succeed");

    match outcome {
        ImportOutcome::Imported { agents, .. } => {
            assert_eq!(agents, 2, "should report 2 imported agents");
        }
        ImportOutcome::Skipped { reason } => panic!("expected Imported, got Skipped: {reason}"),
    }

    let tenant_id = default_tenant_id(&catalog).await;
    let agents_repo = catalog.agents();
    let planner = agents_repo
        .get_by_name(&tenant_id, "planner")
        .await
        .expect("planner exists");
    assert_eq!(planner.model, "claude-x");
    assert_eq!(planner.system_prompt, "plan!");

    let coder = agents_repo
        .get_by_name(&tenant_id, "coder")
        .await
        .expect("coder exists");
    assert_eq!(coder.model, "claude-y");
    assert_eq!(coder.system_prompt, "code!");
}

#[tokio::test]
async fn re_running_import_is_idempotent() {
    let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
    let mut config = base_config();
    config
        .agent_types
        .insert("a".into(), agent_with("m1", None));
    config
        .agent_types
        .insert("b".into(), agent_with("m2", None));

    let first = import_toml_seed(&catalog, &config)
        .await
        .expect("first import");
    assert!(matches!(first, ImportOutcome::Imported { agents: 2, .. }));

    // Mutate the in-memory SimulacraConfig: add a third agent.
    config
        .agent_types
        .insert("c".into(), agent_with("m3", None));

    let second = import_toml_seed(&catalog, &config)
        .await
        .expect("second import");
    assert!(
        matches!(second, ImportOutcome::Skipped { .. }),
        "second run should skip"
    );

    let tenant_id = default_tenant_id(&catalog).await;
    let agents_repo = catalog.agents();
    let listed = agents_repo
        .list(&tenant_id, simulacra_catalog::PageRequest::default(), None)
        .await
        .expect("list");
    assert_eq!(
        listed.items.len(),
        2,
        "should still have only the original 2 agents (c not imported)"
    );
    assert!(
        agents_repo.get_by_name(&tenant_id, "c").await.is_err(),
        "agent 'c' must NOT have been imported on the second pass"
    );
}

#[tokio::test]
async fn import_creates_default_memory_pool_when_memory_section_present() {
    let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
    let mut config = base_config();
    config.agent_types.insert("a".into(), agent_with("m", None));
    config.memory = Some(MemoryConfig {
        dir: PathBuf::from("./.x"),
        tenant: "cli".into(),
        retention: None,
        on_model_change: OnModelChange::Refuse,
    });

    let outcome = import_toml_seed(&catalog, &config).await.expect("import");
    match outcome {
        ImportOutcome::Imported { memory_pools, .. } => {
            assert_eq!(memory_pools, 1, "should create 1 memory pool")
        }
        other => panic!("expected Imported, got {other:?}"),
    }

    let tenant_id = default_tenant_id(&catalog).await;
    let pools_repo = catalog.memory_pools();
    let pools = pools_repo.list(&tenant_id).await.expect("list pools");
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].name, "default");
}

#[tokio::test]
async fn import_skips_default_memory_pool_when_memory_section_absent() {
    let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
    let mut config = base_config();
    config.agent_types.insert("a".into(), agent_with("m", None));
    // memory left as None.

    let outcome = import_toml_seed(&catalog, &config).await.expect("import");
    match outcome {
        ImportOutcome::Imported { memory_pools, .. } => {
            assert_eq!(memory_pools, 0, "no memory pools should be created")
        }
        other => panic!("expected Imported, got {other:?}"),
    }

    let tenant_id = default_tenant_id(&catalog).await;
    let pools_repo = catalog.memory_pools();
    let pools = pools_repo.list(&tenant_id).await.expect("list pools");
    assert!(pools.is_empty(), "no memory pools without [memory] section");
}

#[tokio::test]
async fn import_marks_seeds_applied() {
    let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
    let mut config = base_config();
    config.agent_types.insert("a".into(), agent_with("m", None));

    assert!(
        !catalog
            .is_seed_applied("toml:agent_types")
            .await
            .expect("query seed"),
        "seed must not be applied before import"
    );

    let _ = import_toml_seed(&catalog, &config).await.expect("import");

    assert!(
        catalog
            .is_seed_applied("toml:agent_types")
            .await
            .expect("query seed"),
        "seed must be applied after import"
    );
}

#[tokio::test]
async fn import_with_empty_agent_types_still_marks_seed_applied() {
    let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
    let config = base_config(); // no agent_types

    let outcome = import_toml_seed(&catalog, &config).await.expect("import");
    match outcome {
        ImportOutcome::Imported { agents, .. } => assert_eq!(agents, 0),
        other => panic!("expected Imported, got {other:?}"),
    }

    assert!(
        catalog
            .is_seed_applied("toml:agent_types")
            .await
            .expect("query seed"),
        "empty agent_types should still mark seed as applied"
    );
}

#[tokio::test]
async fn import_capabilities_converts_to_string_form() {
    let catalog = Catalog::open_in_memory().expect("open in-memory catalog");
    let mut config = base_config();
    let mut agent = agent_with("m", None);
    agent.capabilities = Some(CapabilitiesConfig {
        network: vec!["api.example.com".into(), "net:already-prefixed".into()],
        mcp: vec!["github:*".into()],
        shell: true,
        javascript: true,
        python: false,
        paths_read: vec![],
        paths_write: vec![],
        skill_patterns: vec!["skill:rust-*".into()],
        memory: None,
    });
    config.agent_types.insert("x".into(), agent);

    let _ = import_toml_seed(&catalog, &config).await.expect("import");

    let tenant_id = default_tenant_id(&catalog).await;
    let resolved = catalog
        .agents()
        .resolve(&tenant_id, "x")
        .await
        .expect("resolve x");

    let caps = resolved.capabilities;
    assert!(
        caps.contains(&"shell:exec".to_string()),
        "shell:exec missing: {caps:?}"
    );
    assert!(
        caps.contains(&"javascript".to_string()),
        "javascript missing: {caps:?}"
    );
    assert!(
        !caps.contains(&"python".to_string()),
        "python should be absent: {caps:?}"
    );
    // Network: bare host gets `net:` prefix; already-prefixed stays as-is.
    assert!(
        caps.contains(&"net:api.example.com".to_string()),
        "expected net:api.example.com in caps: {caps:?}"
    );
    assert!(
        caps.contains(&"net:already-prefixed".to_string()),
        "expected net:already-prefixed in caps: {caps:?}"
    );
    assert!(
        caps.contains(&"mcp:github:*".to_string()),
        "expected mcp:github:* in caps: {caps:?}"
    );
    assert!(
        caps.contains(&"skill:rust-*".to_string()),
        "expected skill:rust-* in caps: {caps:?}"
    );
}
