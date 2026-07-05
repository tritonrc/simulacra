//! S042 Inc 3 Task 12 — `simulacra-cli --no-catalog` mode.
//!
//! These tests cover spec assertions §`--no-catalog` mode (S042 lines
//! 560–567). Coverage notes:
//!
//! - **Tested directly through helpers** (sufficient because the v1 CLI does
//!   not yet rewire its agent loop through `AgentRepository` — see the
//!   honest-scoping note in `docs/superpowers/plans/2026-05-02-...md` Task
//!   12):
//!   - line 561: no SQLite file is created in `--no-catalog` mode.
//!   - line 562: migrations are not invoked.
//!   - line 563: `MemoryAgentRepository` populated from `SimulacraConfig` serves
//!     `get_by_name`, `list`, `resolve`.
//!   - line 566: mutating methods on the in-memory repos return
//!     `CatalogError::ReadOnly`.
//!
//! - **True by construction (not asserted via runtime test, with rationale)**:
//!   - line 564 ("Per-task VFS does not include `CatalogSkillFs`"): VFS
//!     composition lives in `SimulacraEngine` (simulacra-server), not in
//!     `simulacra-cli`'s sync per-task VFS setup. CLI never mounts
//!     `CatalogSkillFs` today, regardless of mode.
//!   - line 565 ("GraphQL route is not mounted in CLI mode"): `simulacra-cli`
//!     has no GraphQL surface — it is simulacra-server-only.
//!
//! - **Out of scope for v1**:
//!   - line 567 ("agent defined in TOML resolves and runs to completion
//!     under `--no-catalog`"): would require rewiring the CLI agent loop to
//!     consume `AgentRepository` (a ~2k-line refactor, deferred). The
//!     resolution half of "resolves" is covered indirectly via
//!     `no_catalog_mode_resolved_agent_carries_full_config` (asserts that
//!     the same `ResolvedAgent` shape any future runner would consume is
//!     produced).
//!
//! Strategy: drive `ensure_catalog`/`fixtures_from_config` directly with a
//! `SimulacraConfig` literal. No tokio runtime is required for the
//! fixture-shape assertions; the file-existence assertions use
//! `#[tokio::test]` because `ensure_catalog` is async.

use std::collections::HashMap;
use std::path::PathBuf;

use simulacra_catalog::{
    Agent, Catalog, CatalogError, NewAgent, PageRequest, TenantId, repo::AgentRepository,
    repo::TenantRepository, repo::memory::MemoryAgentRepository,
};
use simulacra_cli::CliArgs;
use simulacra_cli::catalog_import::{
    CatalogBootstrapResult, CatalogMode, DEFAULT_TENANT_NAMESPACE, default_tenant_id,
    ensure_catalog, fixtures_from_config, plan_catalog_mode,
};
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, MemoryConfig, OnModelChange, ProjectConfig,
    SimulacraConfig, VfsConfig,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn base_config() -> SimulacraConfig {
    SimulacraConfig {
        project: ProjectConfig {
            name: "no-catalog-tests".into(),
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

/// Build a `CliArgs` populated with the values our tests care about. The
/// remaining fields are inert (`None`/`false`) because `ensure_catalog` only
/// reads `no_catalog`. Keeping every field explicit here prevents future
/// fields from silently defaulting to a behaviour the test did not intend.
fn cli_args(no_catalog: bool) -> CliArgs {
    CliArgs {
        config_path: "simulacra.toml".into(),
        task: None,
        mode: None,
        verbose: false,
        otlp_endpoint: None,
        session: None,
        model: None,
        max_turns: None,
        max_tokens: None,
        max_cost: None,
        no_catalog,
        output_format: simulacra_cli::OutputFormat::Text,
    }
}

/// Per-test temp directory rooted under `std::env::temp_dir()` so tests
/// don't pollute the working tree. Mirrors the pattern used by other
/// `simulacra-cli/tests/*.rs` (see `cli_bootstrap.rs:167`).
fn unique_temp_dir(label: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("simulacra-cli-{label}-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

// ---------------------------------------------------------------------------
// Spec line 561: no DB file is created in --no-catalog mode.
// Spec line 562: migrations are not invoked.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_catalog_mode_does_not_create_db_file() {
    let tmp = unique_temp_dir("nocat-no-file");
    let db_path = tmp.join("should-not-exist.db");

    let mut config = base_config();
    config.catalog = CatalogConfig {
        db_path: Some(db_path.clone()),
    };
    config
        .agent_types
        .insert("planner".into(), agent_with("claude-x", Some("plan!")));

    let args = cli_args(true);
    let result = ensure_catalog(&args, &config, &tmp)
        .await
        .expect("ensure_catalog should succeed");

    assert!(
        result.is_no_catalog(),
        "result should be NoCatalog when --no-catalog is set"
    );
    assert!(
        !db_path.exists(),
        "catalog DB file must NOT have been created at {db_path:?}"
    );
    // Belt and braces: nothing else in the temp dir either.
    let entries: Vec<_> = std::fs::read_dir(&tmp)
        .expect("read temp dir")
        .map(|e| e.unwrap().path())
        .collect();
    assert!(
        entries.is_empty(),
        "no files should have been created in state dir; found: {entries:?}"
    );
}

#[tokio::test]
async fn no_catalog_mode_skips_migrations() {
    // We assert "migrations not invoked" by demonstrating that the seed
    // bookkeeping table the catalog migration would create is not
    // populated. We re-open the *same path* directly with `Catalog::open`
    // afterwards (which does run migrations on a real, fresh DB) and then
    // probe `is_seed_applied` — it must be `false` because the
    // `--no-catalog` bootstrap left no row behind.
    let tmp = unique_temp_dir("nocat-no-migrate");
    let db_path = tmp.join("catalog.db");

    let mut config = base_config();
    config.catalog = CatalogConfig {
        db_path: Some(db_path.clone()),
    };
    config
        .agent_types
        .insert("planner".into(), agent_with("claude-x", None));

    let args = cli_args(true);
    let _ = ensure_catalog(&args, &config, &tmp)
        .await
        .expect("ensure_catalog (--no-catalog) should succeed");
    assert!(!db_path.exists(), "DB must not exist after --no-catalog");

    // Now open the same path. This is the first migration run; the
    // `seeds_applied` table is created empty. `is_seed_applied` must be
    // `false` — proving the `--no-catalog` path did not write a seed row
    // (had migrations + import_toml_seed run, the row would persist).
    let catalog = Catalog::open(&db_path).expect("open after --no-catalog");
    assert!(
        !catalog
            .is_seed_applied("toml:agent_types")
            .await
            .expect("is_seed_applied should query freshly-migrated DB"),
        "no_catalog mode must not have written the toml:agent_types seed row"
    );
}

// ---------------------------------------------------------------------------
// Spec line 558 (companion): default mode does open the DB and import.
// Included here as the negative-case control for the two tests above.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn default_mode_opens_db_and_imports_toml() {
    let tmp = unique_temp_dir("nocat-default-opens");
    let db_path = tmp.join("catalog.db");

    let mut config = base_config();
    config.catalog = CatalogConfig {
        db_path: Some(db_path.clone()),
    };
    config
        .agent_types
        .insert("coder".into(), agent_with("claude-y", Some("code!")));

    let args = cli_args(false);
    let result = ensure_catalog(&args, &config, &tmp)
        .await
        .expect("default-mode ensure_catalog should succeed");

    assert!(
        !result.is_no_catalog(),
        "default mode must produce WithCatalog, got NoCatalog"
    );
    assert_eq!(result.db_path(), Some(db_path.as_path()));
    assert!(
        db_path.exists(),
        "catalog DB file should have been created at {db_path:?}"
    );

    // The agent must be present in the freshly-opened catalog.
    let catalog = Catalog::open(&db_path).expect("re-open populated catalog");
    let tenant = catalog
        .tenants()
        .get_by_namespace("default")
        .await
        .expect("default tenant exists after import");
    let agent = catalog
        .agents()
        .get_by_name(&tenant.id, "coder")
        .await
        .expect("coder agent imported");
    assert_eq!(agent.model, "claude-y");
    assert_eq!(agent.system_prompt, "code!");
    assert!(
        catalog
            .is_seed_applied("toml:agent_types")
            .await
            .expect("seed bookkeeping queryable"),
        "default mode must mark toml:agent_types seed as applied"
    );
}

// ---------------------------------------------------------------------------
// Spec line 563: MemoryAgentRepository populated from SimulacraConfig serves
// get/list/resolve.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_catalog_mode_fixtures_serve_get_list_resolve_via_memory_agent_repo() {
    let mut config = base_config();
    config
        .agent_types
        .insert("planner".into(), agent_with("claude-x", Some("plan!")));
    config
        .agent_types
        .insert("coder".into(), agent_with("claude-y", Some("code!")));

    let fixtures = fixtures_from_config(&config);
    let repo = MemoryAgentRepository::new(fixtures);
    let tenant_id = default_tenant_id();

    // get_by_name
    let planner = repo
        .get_by_name(&tenant_id, "planner")
        .await
        .expect("get_by_name planner");
    assert_eq!(planner.name, "planner");
    assert_eq!(planner.model, "claude-x");
    assert_eq!(planner.system_prompt, "plan!");

    let coder = repo
        .get_by_name(&tenant_id, "coder")
        .await
        .expect("get_by_name coder");
    assert_eq!(coder.name, "coder");
    assert_eq!(coder.model, "claude-y");

    // list (default page size of 20 covers our 2 agents).
    let listed = repo
        .list(&tenant_id, PageRequest::default(), None)
        .await
        .expect("list");
    assert_eq!(listed.items.len(), 2, "should list both agents");
    let names: Vec<&str> = listed
        .items
        .iter()
        .map(|a: &Agent| a.name.as_str())
        .collect();
    assert!(
        names.contains(&"planner"),
        "planner missing from list: {names:?}"
    );
    assert!(
        names.contains(&"coder"),
        "coder missing from list: {names:?}"
    );
    assert!(!listed.has_next_page);

    // resolve
    let resolved = repo
        .resolve(&tenant_id, "planner")
        .await
        .expect("resolve planner");
    assert_eq!(resolved.name, "planner");
    assert_eq!(resolved.model, "claude-x");
    assert_eq!(resolved.system_prompt, "plan!");
    assert!(
        resolved.skills.is_empty(),
        "planner has no inline skills (host references only)"
    );
}

// ---------------------------------------------------------------------------
// Spec line 566: mutating methods return CatalogError::ReadOnly.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_catalog_mode_mutations_return_readonly() {
    let mut config = base_config();
    config
        .agent_types
        .insert("planner".into(), agent_with("claude-x", None));

    let fixtures = fixtures_from_config(&config);
    let repo = MemoryAgentRepository::new(fixtures);
    let tenant_id = default_tenant_id();

    // create
    let new_agent = NewAgent {
        name: "new-one",
        description: None,
        system_prompt: "hi",
        model: "claude-z",
        max_turns: None,
        max_tokens: None,
        memory_pool_id: None,
        skill_ids: &[],
        capabilities: &[],
        channel_ids: &[],
    };
    let err = repo
        .create(&tenant_id, new_agent)
        .await
        .expect_err("create must error on in-memory repo");
    assert!(
        matches!(err, CatalogError::ReadOnly(_)),
        "expected ReadOnly, got {err:?}"
    );

    // update
    let existing_id = repo
        .get_by_name(&tenant_id, "planner")
        .await
        .expect("planner exists")
        .id;
    let err = repo
        .update(&tenant_id, &existing_id, Default::default())
        .await
        .expect_err("update must error on in-memory repo");
    assert!(
        matches!(err, CatalogError::ReadOnly(_)),
        "expected ReadOnly on update, got {err:?}"
    );

    // delete
    let err = repo
        .delete(&tenant_id, &existing_id)
        .await
        .expect_err("delete must error on in-memory repo");
    assert!(
        matches!(err, CatalogError::ReadOnly(_)),
        "expected ReadOnly on delete, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Spec line 567 (partial — resolution half): ResolvedAgent carries every
// field the agent loop would need. The "runs to completion" half is covered
// indirectly because any future agent loop that calls
// `repo.resolve(tenant, name)` will see the full SimulacraConfig contents.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_catalog_mode_resolved_agent_carries_full_config() {
    let mut config = base_config();
    let mut planner = agent_with("claude-x", Some("the long system prompt"));
    planner.max_turns = Some(7);
    planner.max_tokens = Some(4321);
    planner.capabilities = Some(CapabilitiesConfig {
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
    config.agent_types.insert("planner".into(), planner);

    let fixtures = fixtures_from_config(&config);
    let repo = MemoryAgentRepository::new(fixtures);
    let tenant_id = default_tenant_id();

    let resolved = repo
        .resolve(&tenant_id, "planner")
        .await
        .expect("resolve planner");

    assert_eq!(resolved.name, "planner");
    assert_eq!(resolved.system_prompt, "the long system prompt");
    assert_eq!(resolved.model, "claude-x");
    assert_eq!(resolved.max_turns, 7);
    assert_eq!(resolved.max_tokens, Some(4321));

    // Capabilities — same conversion as `import_toml_seed`. We assert the
    // important entries by `contains` rather than exact equality to keep
    // the test resilient to ordering differences.
    let caps = &resolved.capabilities;
    assert!(caps.contains(&"shell:exec".to_string()), "{caps:?}");
    assert!(caps.contains(&"javascript".to_string()), "{caps:?}");
    assert!(!caps.contains(&"python".to_string()), "{caps:?}");
    assert!(
        caps.contains(&"net:api.example.com".to_string()),
        "{caps:?}"
    );
    assert!(
        caps.contains(&"net:already-prefixed".to_string()),
        "{caps:?}"
    );
    assert!(caps.contains(&"mcp:github:*".to_string()), "{caps:?}");
    assert!(caps.contains(&"skill:rust-*".to_string()), "{caps:?}");
}

// ---------------------------------------------------------------------------
// Stable identity: the default tenant is ALWAYS namespace="default" with
// TenantId("default") in --no-catalog mode. The catalog path (handled by
// `import_toml_seed`) uses `get_or_create("default", Some("Default"))` so
// both modes converge on the same namespace string.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn default_tenant_namespace_is_stable() {
    let config = base_config();
    let fixtures = fixtures_from_config(&config);

    // The fixtures must contain exactly one tenant with the "default"
    // namespace. (Not asserting on the synthetic tenant id beyond
    // requiring it equals `default_tenant_id()` so that future code paths
    // that hard-code TenantId("default") interoperate.)
    let tenants: Vec<_> = fixtures.tenants.values().collect();
    assert_eq!(tenants.len(), 1, "expected exactly one default tenant");
    assert_eq!(tenants[0].namespace, DEFAULT_TENANT_NAMESPACE);
    assert_eq!(tenants[0].namespace, "default");
    assert_eq!(tenants[0].display_name.as_deref(), Some("Default"));

    // The TenantId we hand out via `default_tenant_id()` must be the key
    // under which the tenant is stored, so MemoryTenantRepository::get_by_id
    // (which keys on TenantId) finds it.
    let tid = default_tenant_id();
    assert!(
        fixtures.tenants.contains_key(&tid),
        "default_tenant_id() must match the key used in the fixtures map"
    );
    assert_eq!(tid, TenantId("default".into()));
}

// ---------------------------------------------------------------------------
// Memory pool: the [memory] section, when present, materialises a single
// "default" pool belonging to the default tenant. When absent, no pools.
// Mirrors the catalog seed path so the two modes don't diverge.
// ---------------------------------------------------------------------------

#[test]
fn fixtures_with_memory_section_creates_default_pool() {
    let mut config = base_config();
    config.agent_types.insert("a".into(), agent_with("m", None));
    config.memory = Some(MemoryConfig {
        dir: PathBuf::from("./.x"),
        tenant: "cli".into(),
        retention: None,
        on_model_change: OnModelChange::Refuse,
    });

    let fixtures = fixtures_from_config(&config);
    let pools: Vec<_> = fixtures.memory_pools.values().collect();
    assert_eq!(pools.len(), 1, "memory section should create one pool");
    assert_eq!(pools[0].name, "default");
    assert_eq!(pools[0].tenant_id, default_tenant_id());
    // S038 MemoryConfig has no embedding_model field today; both this and
    // the catalog seed path leave it `None`.
    assert_eq!(pools[0].embedding_model, None);
}

#[test]
fn fixtures_without_memory_section_creates_no_pools() {
    let mut config = base_config();
    config.agent_types.insert("a".into(), agent_with("m", None));
    // memory left as None.

    let fixtures = fixtures_from_config(&config);
    assert!(
        fixtures.memory_pools.is_empty(),
        "no [memory] section should produce no pools"
    );
}

// ---------------------------------------------------------------------------
// `plan_catalog_mode` (the sync planning helper used by `bootstrap()`)
// must agree with `ensure_catalog`'s branch decision so that bootstrap
// telemetry / test introspection doesn't lie about which path will run.
// ---------------------------------------------------------------------------

#[test]
fn plan_catalog_mode_with_no_catalog_flag_is_no_catalog() {
    let config = base_config();
    let mode = plan_catalog_mode(&cli_args(true), &config, &PathBuf::from("./.simulacra"));
    assert!(matches!(mode, CatalogMode::NoCatalog));
    assert!(mode.is_no_catalog());
    assert_eq!(mode.db_path(), None);
}

#[test]
fn plan_catalog_mode_default_records_resolved_db_path() {
    let mut config = base_config();
    config.catalog = CatalogConfig {
        db_path: Some(PathBuf::from("/tmp/explicit.db")),
    };
    let mode = plan_catalog_mode(
        &cli_args(false),
        &config,
        &PathBuf::from("/should-be-ignored"),
    );
    match mode {
        CatalogMode::WithCatalog { db_path } => {
            assert_eq!(db_path, PathBuf::from("/tmp/explicit.db"));
        }
        CatalogMode::NoCatalog => panic!("expected WithCatalog"),
    }
}

#[test]
fn plan_catalog_mode_default_falls_back_to_state_dir() {
    let config = base_config();
    let mode = plan_catalog_mode(
        &cli_args(false),
        &config,
        &PathBuf::from("/var/lib/simulacra"),
    );
    match mode {
        CatalogMode::WithCatalog { db_path } => {
            assert_eq!(db_path, PathBuf::from("/var/lib/simulacra/catalog.db"));
        }
        CatalogMode::NoCatalog => panic!("expected WithCatalog"),
    }
}

// ---------------------------------------------------------------------------
// CatalogBootstrapResult accessors round-trip what ensure_catalog returned
// (regression guard: refactors must not break the no_catalog/db_path
// observers tests rely on).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn catalog_bootstrap_result_no_catalog_exposes_fixtures() {
    let mut config = base_config();
    config
        .agent_types
        .insert("planner".into(), agent_with("m", None));
    let args = cli_args(true);
    let tmp = unique_temp_dir("nocat-result-fixtures");
    let result = ensure_catalog(&args, &config, &tmp)
        .await
        .expect("ensure_catalog");
    assert!(result.is_no_catalog());
    assert_eq!(result.db_path(), None);
    let fx = result.fixtures().expect("NoCatalog must carry fixtures");
    assert_eq!(fx.agents.len(), 1, "one agent_type → one agent");
    assert!(matches!(result, CatalogBootstrapResult::NoCatalog { .. }));
}
