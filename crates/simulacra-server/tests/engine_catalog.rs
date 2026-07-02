//! S042 Task 10 — `SimulacraEngine` × `AgentRepository` integration.
//!
//! Spec assertions covered (specs/S042-agent-catalog-graphql.md §"`SimulacraEngine`
//! integration", lines 545-552):
//!
//!   1. `spawn_task` resolves agent from catalog, not from `SimulacraConfig.agent_types`.
//!   2. Agent unknown to catalog returns `EngineError::AgentNotFound` (no panic).
//!   3. Per-task VFS exposes catalog skills at `/skills/<name>/SKILL.md`
//!      and the compatibility `/var/skills/<name>.md` path.
//!   4. Capabilities from catalog feed the per-task capability checker.
//!   5. Memory pool config from catalog routes the task's memory store.
//!   6. Catalog mutations during a running task do not affect that task.
//!   7. Two concurrent tasks for two different agents see their own
//!      skills, capabilities, and memory pool.
//!
//! ─────────────────────────────────────────────────────────────────────────
//! Phase 2 must add the following test-only accessors on `SimulacraEngine` so
//! these tests can prove the *running task's* state (not just that the
//! catalog returned the right values). The current `debug_workspace_snapshot`
//! returns the **raw** `MemoryFs` workspace layer — it does NOT include the
//! catalog skill snapshots, the `MemoryStoreFs` mount at `/var/memory/`,
//! the capability token, etc. Proving "running agent IS X"
//! requires direct accessors:
//!
//!   pub fn debug_resolved_agent(&self, task_id: &str)
//!       -> Option<simulacra_catalog::ResolvedAgent>;
//!   pub fn debug_capability_token(&self, task_id: &str)
//!       -> Option<simulacra_types::CapabilityToken>;
//!   pub fn debug_composed_vfs(&self, task_id: &str)
//!       -> Option<Arc<dyn simulacra_types::VirtualFs>>;
//!
//! `debug_resolved_agent` is the snapshot the engine took at spawn time;
//! the catalog mutation test relies on this snapshot being immutable wrt
//! later catalog updates. `debug_composed_vfs` returns the full per-task
//! VFS stack so we can read `/skills/*/SKILL.md` and `/var/skills/*.md`
//! and observe that the catalog skills are mounted, not just resolvable
//! from the catalog. `debug_capability_token` lets us prove that
//! `resolved.capabilities` was lifted into the running task's
//! `CapabilityToken` rather than discarded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use simulacra_catalog::repo::{
    AgentFileRepository, AgentRepository, MemoryPoolRepository, SkillRepository, TenantRepository,
};
use simulacra_catalog::{
    AgentPatch, Catalog, MemoryPool, NewAgent, NewAgentFile, NewMemoryPool, NewSkill, Skill,
    SkillId, SkillPatch, Tenant,
};
use simulacra_config::{CatalogConfig, ProjectConfig, SimulacraConfig, VfsConfig};
use simulacra_server::{
    BudgetPoolConfig, EngineError, SimulacraEngine, TaskManager, TaskState, TenantConfig,
};
use simulacra_types::{VfsError, VirtualFs};

// ─── Fixtures ────────────────────────────────────────────────────────────

fn catalog_backed_config() -> SimulacraConfig {
    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-engine-catalog-tests".to_string(),
            description: None,
        },
        // INTENTIONALLY EMPTY: assertion #1 says spawn must come from the
        // catalog even when `agent_types` is empty. If a test passes with
        // a populated `agent_types`, it can't tell whether spawn used the
        // catalog or the legacy config path.
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

fn build_engine(config: SimulacraConfig, catalog: &Catalog) -> SimulacraEngine {
    SimulacraEngine::new(
        config,
        None,
        Arc::new(catalog.agents()) as Arc<dyn AgentRepository>,
        Arc::new(catalog.skills()) as Arc<dyn SkillRepository>,
        Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>,
        Arc::new(catalog.tenants()) as Arc<dyn TenantRepository>,
    )
    .expect("engine should construct")
}

async fn create_tenant(catalog: &Catalog, namespace: &str) -> Tenant {
    catalog
        .tenants()
        .create(namespace, Some(namespace))
        .await
        .expect("tenant should be created")
}

async fn create_skill(catalog: &Catalog, tenant: &Tenant, name: &str, body: &str) -> Skill {
    catalog
        .skills()
        .create(
            &tenant.id,
            NewSkill {
                name,
                description: Some("test skill"),
                body,
                metadata: None,
            },
        )
        .await
        .expect("skill should be created")
}

async fn create_memory_pool(
    catalog: &Catalog,
    tenant: &Tenant,
    name: &str,
    store_path: &str,
) -> MemoryPool {
    let config = json!({ "store_path": store_path });
    catalog
        .memory_pools()
        .create(
            &tenant.id,
            NewMemoryPool {
                name,
                embedding_model: Some("text-embed-3-small"),
                config: &config,
            },
        )
        .await
        .expect("memory pool should be created")
}

#[allow(clippy::too_many_arguments)]
async fn create_agent(
    catalog: &Catalog,
    tenant: &Tenant,
    name: &str,
    system_prompt: &str,
    skill_ids: &[SkillId],
    capabilities: &[String],
    memory_pool_id: Option<&simulacra_catalog::MemoryPoolId>,
) -> simulacra_catalog::Agent {
    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name,
                description: Some("test agent"),
                system_prompt,
                model: "ollama:llama3",
                max_turns: Some(8),
                max_tokens: Some(4096),
                memory_pool_id,
                skill_ids,
                capabilities,
                channel_ids: &[],
            },
        )
        .await
        .expect("agent should be created")
}

async fn create_agent_file(
    catalog: &Catalog,
    tenant: &Tenant,
    agent: &simulacra_catalog::Agent,
    name: &str,
    mime_type: &str,
    bytes: &[u8],
) -> simulacra_catalog::AgentFile {
    catalog
        .agent_files()
        .create(
            &tenant.id,
            NewAgentFile {
                agent_id: &agent.id,
                name,
                mime_type,
                bytes,
            },
        )
        .await
        .expect("agent file should be created")
}

fn read_utf8(fs: &dyn VirtualFs, path: &str) -> String {
    String::from_utf8(fs.read(path).expect("path should be readable")).expect("file should be utf8")
}

// ─── Assertion 1: spawn_task resolves from catalog, not from SimulacraConfig ───

#[tokio::test]
async fn spawn_task_resolves_agent_from_catalog_when_config_agent_types_is_empty() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let pool = create_memory_pool(&catalog, &tenant, "main", "/tmp/catalog-memory").await;
    let skill = create_skill(&catalog, &tenant, "noop", "just say hi").await;
    let capabilities = vec!["net:read".to_string()];
    create_agent(
        &catalog,
        &tenant,
        "catalog-worker",
        "from-catalog",
        std::slice::from_ref(&skill.id),
        &capabilities,
        Some(&pool.id),
    )
    .await;

    // catalog_backed_config has agent_types: {} — if the engine consulted
    // SimulacraConfig.agent_types it would return AgentTypeNotFound.
    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();

    let handle = engine
        .spawn_task(
            &manager,
            "use the catalog-backed agent",
            &tenant_config("acme", "catalog-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("catalog-backed spawn must succeed even with empty config.agent_types");

    assert_eq!(handle.state, TaskState::Pending);
    // The handle must report the catalog agent name; this proves the agent
    // type the worker received was the resolved name from the catalog row.
    assert_eq!(handle.agent_type, "catalog-worker");

    // Phase 2 must add: SimulacraEngine::debug_resolved_agent(task_id)
    //   -> Option<simulacra_catalog::ResolvedAgent>
    // The engine's per-task snapshot of the resolved agent (the value
    // actually used to build the AgentLoop). Any field on this struct must
    // match what the catalog returned at spawn time.
    let resolved = engine
        .debug_resolved_agent(&handle.task_id)
        .expect("engine must record the resolved agent for the task");

    assert_eq!(resolved.name, "catalog-worker");
    assert_eq!(resolved.system_prompt, "from-catalog");
    assert_eq!(resolved.model, "ollama:llama3");
    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.skills[0].name, "noop");
    assert_eq!(resolved.capabilities, vec!["net:read".to_string()]);
    assert_eq!(
        resolved
            .memory_pool
            .as_ref()
            .expect("resolved should carry the catalog memory pool")
            .name,
        "main"
    );
}

// ─── Assertion 2: unknown agent → EngineError::AgentNotFound (no panic) ───

#[tokio::test]
async fn spawn_task_with_unknown_agent_returns_agent_not_found_error() {
    let catalog = Catalog::open_in_memory().unwrap();
    create_tenant(&catalog, "acme").await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();

    let error = engine
        .spawn_task(
            &manager,
            "missing catalog agent",
            &tenant_config("acme", "missing"),
            Some("missing"),
            json!({}),
            None,
            None,
        )
        .await
        .expect_err("unknown catalog agents must be rejected with a structured error");

    match error {
        EngineError::AgentNotFound { tenant, agent } => {
            assert_eq!(tenant, "acme");
            assert_eq!(agent, "missing");
        }
        other => panic!("expected EngineError::AgentNotFound, got: {other:?}"),
    }
}

// Additional edge case: an agent named X exists in tenant A — resolving X in
// tenant B must still return AgentNotFound, not silently leak across tenants.
// This protects assertion 2 against a per-name-not-per-tenant impl bug.
#[tokio::test]
async fn spawn_task_does_not_resolve_agent_across_tenants() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant_a = create_tenant(&catalog, "tenant-a").await;
    create_tenant(&catalog, "tenant-b").await;
    create_agent(
        &catalog,
        &tenant_a,
        "shared-name",
        "from-tenant-a",
        &[],
        &[],
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();

    let error = engine
        .spawn_task(
            &manager,
            "try cross-tenant resolve",
            &tenant_config("tenant-b", "shared-name"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect_err("agent in tenant-a must not be resolvable from tenant-b");

    match error {
        EngineError::AgentNotFound { tenant, agent } => {
            assert_eq!(tenant, "tenant-b");
            assert_eq!(agent, "shared-name");
        }
        other => panic!("expected AgentNotFound for cross-tenant lookup, got: {other:?}"),
    }
}

// Edge case: spawn_task against a tenant namespace that does NOT exist in
// the catalog must surface `EngineError::Tenant` (NOT `AgentNotFound`).
// Closes a Phase-4 review gap — the namespace-not-in-catalog branch is
// distinguished from the agent-name-not-in-tenant branch and tests must
// pin the distinction so future refactors don't collapse them.
#[tokio::test]
async fn spawn_task_with_unknown_tenant_namespace_returns_tenant_error() {
    let catalog = Catalog::open_in_memory().unwrap();
    // No tenant created — namespace lookup will return NotFound.

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();

    let error = engine
        .spawn_task(
            &manager,
            "spawn with no tenant",
            &tenant_config("ghost-tenant", "ghost-agent"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect_err("unknown tenant namespace must surface a structured error");

    match error {
        EngineError::Tenant(msg) => {
            assert!(
                msg.contains("ghost-tenant"),
                "Tenant error must reference the missing namespace; got: {msg}"
            );
        }
        other => panic!("expected EngineError::Tenant for unknown namespace, got: {other:?}"),
    }
}

// ─── Assertion 3: per-task VFS exposes catalog skills at /skills and /var/skills ───

#[tokio::test]
async fn catalog_skill_visible_at_var_skills_in_running_task() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let skill_a = create_skill(&catalog, &tenant, "search", "search body").await;
    let skill_b = create_skill(&catalog, &tenant, "summarize", "summarize body").await;
    create_agent(
        &catalog,
        &tenant,
        "catalog-worker",
        "from-catalog",
        &[skill_a.id.clone(), skill_b.id.clone()],
        &[],
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "show mounted skills",
            &tenant_config("acme", "catalog-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should succeed");

    // Phase 2 must add: SimulacraEngine::debug_composed_vfs(task_id)
    //   -> Option<Arc<dyn simulacra_types::VirtualFs>>
    // Returns the *full* composed per-task VFS stack (MemoryFs + MailboxFs +
    // [MemoryStoreFs] + ServiceFs + ProcFs + catalog skill snapshots).
    // The existing debug_workspace_snapshot only returns the raw MemoryFs
    // workspace layer, which does NOT include the catalog skills mount.
    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");

    let mut canonical_listing = vfs
        .list_dir("/skills")
        .expect("/skills must expose catalog skills for S017 discovery");
    canonical_listing.sort();
    assert_eq!(canonical_listing, vec!["search", "summarize"]);

    let search_dir = vfs
        .list_dir("/skills/search")
        .expect("/skills/search must be a skill directory");
    assert_eq!(search_dir, vec!["SKILL.md"]);

    let canonical_search = read_utf8(vfs.as_ref(), "/skills/search/SKILL.md");
    assert!(
        canonical_search.contains("name: search")
            && canonical_search.contains("description: test skill")
            && canonical_search.contains("search body"),
        "/skills/search/SKILL.md must contain valid S017 frontmatter plus body, got: {canonical_search:?}"
    );

    let mut listing = vfs
        .list_dir("/var/skills")
        .expect("/var/skills compatibility path must be mounted from the catalog");
    listing.sort();
    assert_eq!(listing, vec!["search.md", "summarize.md"]);

    // Bodies must round-trip. CatalogSkillFs renders YAML frontmatter; the
    // catalog body is the suffix of the rendered file.
    let search = read_utf8(vfs.as_ref(), "/var/skills/search.md");
    let summarize = read_utf8(vfs.as_ref(), "/var/skills/summarize.md");
    assert!(
        search.contains("search body"),
        "/var/skills/search.md must contain the catalog skill body, got: {search:?}"
    );
    assert!(
        summarize.contains("summarize body"),
        "/var/skills/summarize.md must contain the catalog skill body, got: {summarize:?}"
    );
}

#[tokio::test]
async fn catalog_skill_mounts_are_read_only_for_writes_and_removes() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let skill = create_skill(&catalog, &tenant, "noop", "original body").await;
    create_agent(
        &catalog,
        &tenant,
        "readonly-skill-worker",
        "from-catalog",
        std::slice::from_ref(&skill.id),
        &[],
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "try to mutate mounted skills",
            &tenant_config("acme", "readonly-skill-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should succeed");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");

    for path in [
        "/skills/noop/SKILL.md",
        "/workspace/../skills/noop/SKILL.md",
        "/var/skills/noop.md",
    ] {
        let err = vfs
            .write(path, b"tampered")
            .expect_err("catalog skill snapshots must reject writes");
        assert!(matches!(err, VfsError::PermissionDenied(_)));
    }

    let err = vfs
        .remove("/skills/noop/SKILL.md")
        .expect_err("catalog skill snapshots must reject removes");
    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[tokio::test]
async fn catalog_skill_name_with_path_separator_returns_vfs_error() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let skill = create_skill(&catalog, &tenant, "bad/name", "body").await;
    create_agent(
        &catalog,
        &tenant,
        "unsafe-skill-worker",
        "from-catalog",
        std::slice::from_ref(&skill.id),
        &[],
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let error = engine
        .spawn_task(
            &manager,
            "spawn with unsafe skill name",
            &tenant_config("acme", "unsafe-skill-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect_err("unsafe catalog skill names must fail before VFS mount");

    match error {
        EngineError::VfsError(message) => {
            assert!(
                message.contains("bad/name") && message.contains("invalid catalog skill name"),
                "VFS error should name the unsafe skill, got: {message}"
            );
        }
        other => panic!("expected EngineError::VfsError for unsafe skill name, got: {other:?}"),
    }
}

// Edge case for assertion 3: an agent with zero skills should still spawn
// successfully and `/var/skills` must be mountable (or empty), never panic.
#[tokio::test]
async fn agent_with_no_skills_spawns_and_var_skills_is_empty_or_absent() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    create_agent(
        &catalog,
        &tenant,
        "skill-less",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "no skills",
            &tenant_config("acme", "skill-less"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent with empty skills must spawn");

    let resolved = engine
        .debug_resolved_agent(&handle.task_id)
        .expect("resolved agent must be recorded");
    assert!(
        resolved.skills.is_empty(),
        "agent with no skills must yield empty resolved.skills, got {:?}",
        resolved.skills
    );
}

// ─── S045 Layer 4b: per-task VFS mounts /var/agent_files/ ────────────────

#[tokio::test]
async fn agent_files_are_listed_at_var_agent_files_with_verbatim_names() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let agent = create_agent(
        &catalog,
        &tenant,
        "file-worker",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    create_agent_file(
        &catalog,
        &tenant,
        &agent,
        "payload.bin",
        "application/octet-stream",
        b"bin-bytes",
    )
    .await;
    create_agent_file(
        &catalog,
        &tenant,
        &agent,
        "briefing v1.pdf",
        "application/pdf",
        b"%PDF-test",
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog)
        .with_agent_file_store(catalog.agent_file_store());
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "show mounted agent files",
            &tenant_config("acme", "file-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent with files must spawn");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");
    let mut listing = vfs
        .list_dir("/var/agent_files")
        .expect("/var/agent_files must be mounted for agents with files");
    listing.sort();

    assert_eq!(listing, vec!["briefing v1.pdf", "payload.bin"]);
}

#[tokio::test]
async fn agent_file_bytes_are_read_verbatim_from_var_agent_files() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let agent = create_agent(
        &catalog,
        &tenant,
        "binary-worker",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    let bytes = [0x00, 0x11, 0xFF, 0x7F, b'F', b'o', b'r', b'g', b'e'];
    create_agent_file(
        &catalog,
        &tenant,
        &agent,
        "raw.dat",
        "application/octet-stream",
        &bytes,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog)
        .with_agent_file_store(catalog.agent_file_store());
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "read binary agent file",
            &tenant_config("acme", "binary-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent with binary file must spawn");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");
    let served = vfs
        .read("/var/agent_files/raw.dat")
        .expect("mounted agent file must be readable");

    assert_eq!(served, bytes.to_vec());
}

#[tokio::test]
async fn unknown_agent_file_name_returns_not_found() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let agent = create_agent(
        &catalog,
        &tenant,
        "unknown-file-worker",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    create_agent_file(
        &catalog,
        &tenant,
        &agent,
        "known.txt",
        "text/plain",
        b"known",
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog)
        .with_agent_file_store(catalog.agent_file_store());
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "read unknown agent file",
            &tenant_config("acme", "unknown-file-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent with files must spawn");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");
    let err = vfs
        .read("/var/agent_files/missing.txt")
        .expect_err("unknown mounted agent file must not read successfully");

    assert!(matches!(err, VfsError::NotFound(_)));
}

#[tokio::test]
async fn agent_files_mount_is_read_only_for_writes() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let agent = create_agent(
        &catalog,
        &tenant,
        "readonly-writes",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    create_agent_file(
        &catalog,
        &tenant,
        &agent,
        "manual.txt",
        "text/plain",
        b"original",
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog)
        .with_agent_file_store(catalog.agent_file_store());
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "write mounted agent file",
            &tenant_config("acme", "readonly-writes"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent with files must spawn");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");
    let err = vfs
        .write("/var/agent_files/manual.txt", b"mutated")
        .expect_err("/var/agent_files must reject writes");

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

#[tokio::test]
async fn agent_files_mount_is_read_only_for_removes() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let agent = create_agent(
        &catalog,
        &tenant,
        "readonly-removes",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    create_agent_file(
        &catalog,
        &tenant,
        &agent,
        "protected.txt",
        "text/plain",
        b"keep me",
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog)
        .with_agent_file_store(catalog.agent_file_store());
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "remove mounted agent file",
            &tenant_config("acme", "readonly-removes"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent with files must spawn");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");
    let err = vfs
        .remove("/var/agent_files/protected.txt")
        .expect_err("/var/agent_files must reject removes");

    assert!(matches!(err, VfsError::PermissionDenied(_)));
}

// Chosen behavior for the empty case: `/var/agent_files` exists and lists as
// an empty directory so tasks do not need a defensive existence check.
#[tokio::test]
async fn agent_with_no_files_spawns_and_var_agent_files_is_empty_dir() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    create_agent(
        &catalog,
        &tenant,
        "empty-files",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog)
        .with_agent_file_store(catalog.agent_file_store());
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "no agent files",
            &tenant_config("acme", "empty-files"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent with no files must spawn");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");
    let listing = vfs
        .list_dir("/var/agent_files")
        .expect("/var/agent_files must exist even when the agent has zero files");

    assert!(
        listing.is_empty(),
        "expected empty /var/agent_files for an agent with no files, got {listing:?}"
    );
}

#[tokio::test]
async fn agent_file_mount_is_a_spawn_time_snapshot_for_running_task() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let agent = create_agent(
        &catalog,
        &tenant,
        "snapshot-files",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    let original = b"frozen-at-spawn".to_vec();
    let file = create_agent_file(
        &catalog,
        &tenant,
        &agent,
        "handbook.txt",
        "text/plain",
        &original,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog)
        .with_agent_file_store(catalog.agent_file_store());
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "snapshot mounted agent files",
            &tenant_config("acme", "snapshot-files"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent with files must spawn");

    // S045's behavior section governs here: a running task keeps its copy even
    // after the catalog row + bytes are detached post-spawn.
    catalog
        .agent_files()
        .delete(&tenant.id, &file.id)
        .await
        .expect("detaching the file after spawn should succeed");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");
    let served = vfs
        .read("/var/agent_files/handbook.txt")
        .expect("running task must keep the spawn-time file snapshot");

    assert_eq!(served, original);
}

// Chosen behavior without `with_agent_file_store(...)`: spawn succeeds and
// `/var/agent_files` is still present as an empty directory rather than absent.
#[tokio::test]
async fn agent_with_files_spawns_without_agent_file_store_and_var_agent_files_is_empty_dir() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let agent = create_agent(
        &catalog,
        &tenant,
        "no-store-files",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    create_agent_file(
        &catalog,
        &tenant,
        &agent,
        "manual.pdf",
        "application/pdf",
        b"%PDF-no-store",
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "spawn without agent file store",
            &tenant_config("acme", "no-store-files"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn must succeed even when AgentFileStore is not wired");

    let resolved = engine
        .debug_resolved_agent(&handle.task_id)
        .expect("resolved agent must be recorded");
    assert_eq!(resolved.files.len(), 1);
    assert_eq!(resolved.files[0].name, "manual.pdf");

    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("engine must expose the composed VFS for the task");
    let listing = vfs
        .list_dir("/var/agent_files")
        .expect("/var/agent_files must be empty instead of missing without a file store");

    assert!(
        listing.is_empty(),
        "without with_agent_file_store, /var/agent_files should be empty, got {listing:?}"
    );
}

// ─── Assertion 4: capabilities from catalog feed the capability checker ───

#[tokio::test]
async fn catalog_capabilities_are_lifted_into_per_task_capability_token() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    // Mix of network grants (whitelisted hosts) and a no-op shell grant —
    // we want concrete strings so the capability_token comparison is
    // unambiguous about *which* capability flowed through.
    let capabilities = vec![
        "net:read:api.example.com".to_string(),
        "net:read:metrics.example.com".to_string(),
        "skill:rust-*".to_string(),
    ];
    create_agent(
        &catalog,
        &tenant,
        "catalog-worker",
        "from-catalog",
        &[],
        &capabilities,
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "check catalog capabilities",
            &tenant_config("acme", "catalog-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should succeed");

    assert_eq!(handle.state, TaskState::Pending);

    // Resolve via the engine snapshot — proves the value the engine
    // captured matches what the catalog returned.
    let resolved = engine
        .debug_resolved_agent(&handle.task_id)
        .expect("resolved agent must be recorded");
    let mut got_caps = resolved.capabilities.clone();
    got_caps.sort();
    let mut want_caps = capabilities.clone();
    want_caps.sort();
    assert_eq!(got_caps, want_caps);

    // Phase 2 must add: SimulacraEngine::debug_capability_token(task_id)
    //   -> Option<simulacra_types::CapabilityToken>
    // Returns the CapabilityToken that was built for the running task. The
    // network grants from the catalog must show up as NetworkPermission
    // entries — proving the catalog values feed the capability checker
    // (assertion 4) rather than being discarded.
    let token = engine
        .debug_capability_token(&handle.task_id)
        .expect("engine must expose the per-task CapabilityToken");
    let mut net_grants: Vec<String> = token.network.iter().map(|p| p.0.clone()).collect();
    net_grants.sort();
    let mut want_grants = vec![
        "net:read:api.example.com".to_string(),
        "net:read:metrics.example.com".to_string(),
    ];
    want_grants.sort();
    assert_eq!(
        net_grants, want_grants,
        "catalog capabilities must land in CapabilityToken.network"
    );
    assert_eq!(
        token.skill_patterns,
        vec!["skill:rust-*".to_string()],
        "catalog skill capabilities must land in CapabilityToken.skill_patterns"
    );
}

#[tokio::test]
async fn mcp_server_capability_from_ui_expands_to_all_server_tools() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let capabilities = vec!["mcp:fetcher".to_string(), "mcp:github:search".to_string()];
    create_agent(
        &catalog,
        &tenant,
        "catalog-worker",
        "from-catalog",
        &[],
        &capabilities,
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "check mcp capabilities",
            &tenant_config("acme", "catalog-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should succeed");

    let token = engine
        .debug_capability_token(&handle.task_id)
        .expect("engine must expose the per-task CapabilityToken");

    assert_eq!(
        token.mcp_tools,
        vec!["mcp:fetcher:*".to_string(), "mcp:github:search".to_string()],
        "UI server-level grants must become dispatch-ready mcp:<server>:* patterns"
    );
}

// ─── Assertion 5: memory pool config from catalog routes the memory store ─

#[tokio::test]
async fn catalog_memory_pool_routes_per_task_memory_store() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let pool =
        create_memory_pool(&catalog, &tenant, "team-memory", "/tmp/catalog-pool-routed").await;
    create_agent(
        &catalog,
        &tenant,
        "catalog-worker",
        "from-catalog",
        &[],
        &[],
        Some(&pool.id),
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "route memory store",
            &tenant_config("acme", "catalog-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should succeed");

    assert_eq!(handle.state, TaskState::Pending);

    // The engine's per-task resolved-agent snapshot must carry the catalog
    // memory pool whose store_path is the value Phase 2 routes the
    // SqliteMemoryStore at. Asserting the engine snapshot — not just the
    // catalog row — proves the pool flowed into the per-task wiring.
    let resolved = engine
        .debug_resolved_agent(&handle.task_id)
        .expect("resolved agent must be recorded");
    let mp = resolved
        .memory_pool
        .as_ref()
        .expect("resolved agent must carry the catalog memory pool");
    assert_eq!(mp.name, "team-memory");
    assert_eq!(mp.config["store_path"], json!("/tmp/catalog-pool-routed"));
}

// Edge case for assertion 5: agent without a memory pool must still spawn
// and the resolved snapshot must report `memory_pool: None` (the engine
// must not panic or auto-fill a default pool).
#[tokio::test]
async fn agent_without_memory_pool_spawns_and_resolved_pool_is_none() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    create_agent(
        &catalog,
        &tenant,
        "no-memory",
        "from-catalog",
        &[],
        &[],
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "no memory pool",
            &tenant_config("acme", "no-memory"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("agent without memory pool must spawn");

    let resolved = engine
        .debug_resolved_agent(&handle.task_id)
        .expect("resolved agent must be recorded");
    assert!(
        resolved.memory_pool.is_none(),
        "agent created with memory_pool=None must resolve to memory_pool=None"
    );
}

// ─── Assertion 6: catalog mutations during a running task are isolated ───

#[tokio::test]
async fn catalog_mutation_during_running_task_does_not_affect_that_tasks_snapshot() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;
    let skill = create_skill(&catalog, &tenant, "noop", "original body").await;
    let agent = create_agent(
        &catalog,
        &tenant,
        "catalog-worker",
        "original prompt",
        std::slice::from_ref(&skill.id),
        &["net:read".to_string()],
        None,
    )
    .await;

    let engine = build_engine(catalog_backed_config(), &catalog);
    let manager = TaskManager::new();
    let handle = engine
        .spawn_task(
            &manager,
            "snapshot before mutation",
            &tenant_config("acme", "catalog-worker"),
            None,
            json!({}),
            None,
            None,
        )
        .await
        .expect("spawn_task should succeed");

    // Mutate the catalog AFTER the task has been spawned — this is the
    // race window assertion 6 protects against.
    catalog
        .skills()
        .update(
            &tenant.id,
            &skill.id,
            SkillPatch {
                body: Some("changed body"),
                ..SkillPatch::default()
            },
        )
        .await
        .expect("skill update should succeed");
    catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                system_prompt: Some("changed prompt"),
                capabilities: Some(&["shell:exec".to_string()]),
                ..AgentPatch::default()
            },
        )
        .await
        .expect("agent update should succeed");

    // Sanity check: the catalog DOES see the new values (post-mutation).
    let post_mutation = catalog
        .agents()
        .resolve(&tenant.id, "catalog-worker")
        .await
        .expect("catalog should resolve the updated agent");
    assert_eq!(post_mutation.system_prompt, "changed prompt");
    assert_eq!(post_mutation.capabilities, vec!["shell:exec".to_string()]);
    assert_eq!(post_mutation.skills[0].body, "changed body");

    // The running task's resolved-agent snapshot must NOT have moved.
    let snapshot = engine
        .debug_resolved_agent(&handle.task_id)
        .expect("engine must retain the per-task resolved snapshot");
    assert_eq!(
        snapshot.system_prompt, "original prompt",
        "running task's prompt must not change when the catalog row is updated"
    );
    assert_eq!(
        snapshot.capabilities,
        vec!["net:read".to_string()],
        "running task's capabilities must not change when the catalog row is updated"
    );
    assert_eq!(snapshot.skills.len(), 1);
    assert_eq!(
        snapshot.skills[0].body, "original body",
        "running task's mounted skill body must not change when the catalog row is updated"
    );

    // The composed VFS must serve the ORIGINAL body, not the changed one.
    let vfs = engine
        .debug_composed_vfs(&handle.task_id)
        .expect("composed VFS must be available");
    let served = read_utf8(vfs.as_ref(), "/var/skills/noop.md");
    assert!(
        served.contains("original body") && !served.contains("changed body"),
        "/var/skills/noop.md must keep the spawn-time body, got: {served:?}"
    );
    let canonical_served = read_utf8(vfs.as_ref(), "/skills/noop/SKILL.md");
    assert!(
        canonical_served.contains("original body") && !canonical_served.contains("changed body"),
        "/skills/noop/SKILL.md must keep the spawn-time body, got: {canonical_served:?}"
    );

    // And the per-task capability token must NOT have absorbed the new
    // catalog grant.
    let token = engine
        .debug_capability_token(&handle.task_id)
        .expect("capability token must be retained");
    assert!(
        !token.shell,
        "running task must not gain shell:exec grant added after spawn"
    );
}

// ─── Assertion 7: two concurrent tasks see their own state ───

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_tasks_have_isolated_skills_capabilities_and_memory_pool() {
    let catalog = Catalog::open_in_memory().unwrap();
    let tenant = create_tenant(&catalog, "acme").await;

    let skill_a = create_skill(&catalog, &tenant, "skill-a", "body-a").await;
    let skill_b = create_skill(&catalog, &tenant, "skill-b", "body-b").await;
    let pool_a = create_memory_pool(&catalog, &tenant, "pool-a", "/tmp/pool-a").await;
    let pool_b = create_memory_pool(&catalog, &tenant, "pool-b", "/tmp/pool-b").await;

    create_agent(
        &catalog,
        &tenant,
        "agent-a",
        "prompt-a",
        std::slice::from_ref(&skill_a.id),
        &["net:read:a.example.com".to_string()],
        Some(&pool_a.id),
    )
    .await;
    create_agent(
        &catalog,
        &tenant,
        "agent-b",
        "prompt-b",
        std::slice::from_ref(&skill_b.id),
        &["net:read:b.example.com".to_string()],
        Some(&pool_b.id),
    )
    .await;

    let engine = Arc::new(build_engine(catalog_backed_config(), &catalog));
    let manager = Arc::new(TaskManager::new());

    let first = {
        let engine = Arc::clone(&engine);
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            engine
                .spawn_task(
                    &manager,
                    "run agent a",
                    &tenant_config("acme", "agent-a"),
                    None,
                    json!({}),
                    None,
                    None,
                )
                .await
        })
    };
    let second = {
        let engine = Arc::clone(&engine);
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            engine
                .spawn_task(
                    &manager,
                    "run agent b",
                    &tenant_config("acme", "agent-b"),
                    None,
                    json!({}),
                    None,
                    None,
                )
                .await
        })
    };

    let handle_a = first.await.unwrap().expect("agent a should spawn");
    let handle_b = second.await.unwrap().expect("agent b should spawn");
    assert_ne!(
        handle_a.task_id, handle_b.task_id,
        "concurrent spawns must produce distinct task ids"
    );

    let resolved_a = engine
        .debug_resolved_agent(&handle_a.task_id)
        .expect("agent a must have a resolved snapshot");
    let resolved_b = engine
        .debug_resolved_agent(&handle_b.task_id)
        .expect("agent b must have a resolved snapshot");

    // Skills isolation.
    assert_eq!(resolved_a.skills.len(), 1);
    assert_eq!(resolved_a.skills[0].name, "skill-a");
    assert_eq!(resolved_a.skills[0].body, "body-a");
    assert_eq!(resolved_b.skills.len(), 1);
    assert_eq!(resolved_b.skills[0].name, "skill-b");
    assert_eq!(resolved_b.skills[0].body, "body-b");

    // Capabilities isolation.
    assert_eq!(
        resolved_a.capabilities,
        vec!["net:read:a.example.com".to_string()]
    );
    assert_eq!(
        resolved_b.capabilities,
        vec!["net:read:b.example.com".to_string()]
    );

    // Memory pool isolation.
    let mp_a = resolved_a
        .memory_pool
        .as_ref()
        .expect("agent a must carry pool-a");
    let mp_b = resolved_b
        .memory_pool
        .as_ref()
        .expect("agent b must carry pool-b");
    assert_eq!(mp_a.name, "pool-a");
    assert_eq!(mp_b.name, "pool-b");
    assert_eq!(mp_a.config["store_path"], json!("/tmp/pool-a"));
    assert_eq!(mp_b.config["store_path"], json!("/tmp/pool-b"));

    // VFS isolation: each task's composed VFS shows only its own skills.
    let vfs_a = engine
        .debug_composed_vfs(&handle_a.task_id)
        .expect("agent a composed VFS");
    let vfs_b = engine
        .debug_composed_vfs(&handle_b.task_id)
        .expect("agent b composed VFS");
    let mut listing_a = vfs_a
        .list_dir("/var/skills")
        .expect("agent a /var/skills must exist");
    let mut listing_b = vfs_b
        .list_dir("/var/skills")
        .expect("agent b /var/skills must exist");
    listing_a.sort();
    listing_b.sort();
    assert_eq!(listing_a, vec!["skill-a.md"]);
    assert_eq!(listing_b, vec!["skill-b.md"]);

    let mut canonical_listing_a = vfs_a
        .list_dir("/skills")
        .expect("agent a /skills must exist");
    let mut canonical_listing_b = vfs_b
        .list_dir("/skills")
        .expect("agent b /skills must exist");
    canonical_listing_a.sort();
    canonical_listing_b.sort();
    assert_eq!(canonical_listing_a, vec!["skill-a"]);
    assert_eq!(canonical_listing_b, vec!["skill-b"]);

    // CapabilityToken isolation.
    let token_a = engine
        .debug_capability_token(&handle_a.task_id)
        .expect("agent a token");
    let token_b = engine
        .debug_capability_token(&handle_b.task_id)
        .expect("agent b token");
    let nets_a: Vec<String> = token_a.network.iter().map(|p| p.0.clone()).collect();
    let nets_b: Vec<String> = token_b.network.iter().map(|p| p.0.clone()).collect();
    assert_eq!(nets_a, vec!["net:read:a.example.com".to_string()]);
    assert_eq!(nets_b, vec!["net:read:b.example.com".to_string()]);
}
