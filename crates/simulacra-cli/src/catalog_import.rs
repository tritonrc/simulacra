//! S042 Inc 3 Tasks 11 + 12 — TOML↔Catalog plumbing for `simulacra-cli`.
//!
//! Two surfaces live here:
//!
//! 1. [`import_toml_seed`] (Task 11) — one-shot TOML→catalog seeding for
//!    default (catalog-backed) mode. Idempotent via the `seeds_applied`
//!    table; subsequent calls short-circuit with [`ImportOutcome::Skipped`].
//!    Skill names from `AgentTypeConfig.skills` are *references* to
//!    host-mounted skills (S033) — they are NOT seeded as catalog rows.
//!
//! 2. [`fixtures_from_config`] + [`ensure_catalog`] (Task 12) — bootstrap
//!    helpers that pick the catalog vs `--no-catalog` path. In `--no-catalog`
//!    mode no SQLite file is opened or created; the same `SimulacraConfig` is
//!    materialised into a [`SharedFixtures`] bundle that backs the in-memory
//!    repositories (`MemoryAgentRepository`, etc.).
//!
//! Spec: §"CLI modes" (lines 376–386) and §"--no-catalog mode" assertions
//! (lines 560–567) of `specs/S042-agent-catalog-graphql.md`.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;
use simulacra_catalog::{
    Agent, AgentId, Catalog, CatalogError, MemoryPool, MemoryPoolId, NewAgent, NewMemoryPool,
    SkillId, Tenant, TenantId,
    repo::{
        AgentRepository, MemoryPoolRepository, TenantRepository,
        memory::{InMemoryFixtures, SharedFixtures},
    },
};
use simulacra_config::{AgentTypeConfig, CapabilitiesConfig, SimulacraConfig};

use crate::CliArgs;

/// Result of a [`import_toml_seed`] call.
#[derive(Debug)]
pub enum ImportOutcome {
    /// Seed ran for the first time. Counts reflect the number of rows
    /// inserted in this call.
    Imported {
        agents: usize,
        skills: usize,
        memory_pools: usize,
    },
    /// Seed had already been applied; nothing changed. The TOML edit is
    /// preserved as-is for operator inspection but is not propagated to the
    /// catalog (use GraphQL mutations to evolve agents post-bootstrap).
    Skipped { reason: String },
}

/// Errors that can occur during TOML→catalog seeding.
#[derive(Debug)]
pub enum ImportError {
    Catalog(CatalogError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Catalog(e) => write!(f, "catalog error: {e}"),
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImportError::Catalog(e) => Some(e),
        }
    }
}

impl From<CatalogError> for ImportError {
    fn from(e: CatalogError) -> Self {
        ImportError::Catalog(e)
    }
}

const SEED_SOURCE: &str = "toml:agent_types";

/// Seed `config.agent_types` (and `[memory]` if present) into `catalog` under
/// the `default` tenant. Idempotent on repeated invocations.
///
/// On first run:
/// 1. Ensures the `default` tenant exists.
/// 2. Materialises one agent row per `(name, agent_type)` in `config.agent_types`.
/// 3. If `config.memory` is `Some`, creates a single `default` memory pool
///    capturing `embedding_model` and the full `MemoryConfig` as JSON.
/// 4. Marks `"toml:agent_types"` as applied in `seeds_applied`.
///
/// On subsequent runs, returns [`ImportOutcome::Skipped`] without touching
/// the database.
pub async fn import_toml_seed(
    catalog: &Catalog,
    config: &SimulacraConfig,
) -> Result<ImportOutcome, ImportError> {
    if catalog.is_seed_applied(SEED_SOURCE).await? {
        // S042 spec line 557: "change ignored; one INFO log emitted." Carries
        // tenant_id at the namespace level since the seed key is global; per-
        // tenant seeds are a future spec.
        let reason = format!("seed {SEED_SOURCE} already applied");
        tracing::info!(
            tenant_namespace = "default",
            seed = SEED_SOURCE,
            "catalog seed already applied; TOML edits ignored"
        );
        return Ok(ImportOutcome::Skipped { reason });
    }

    let tenants_repo = catalog.tenants();
    let agents_repo = catalog.agents();
    let pools_repo = catalog.memory_pools();

    let tenant = tenants_repo
        .get_or_create("default", Some("Default"))
        .await?;

    let mut agents_imported = 0usize;
    let mut pools_imported = 0usize;

    // Memory pool (single "default" pool when [memory] present).
    if let Some(memory_cfg) = config.memory.as_ref() {
        let already_present = pools_repo.get_by_name(&tenant.id, "default").await.is_ok();
        if !already_present {
            let pool_config = serde_json::to_value(memory_cfg).unwrap_or(Value::Null);
            // S038 MemoryConfig has no embedding_model field today; pools store
            // it as nullable. Future MemoryConfig revisions can plumb it in.
            let embedding_model: Option<&str> = None;
            pools_repo
                .create(
                    &tenant.id,
                    NewMemoryPool {
                        name: "default",
                        embedding_model,
                        config: &pool_config,
                    },
                )
                .await?;
            pools_imported += 1;
        }
    }

    // Agents.
    for (agent_name, agent_cfg) in &config.agent_types {
        // Defensive — seeds_applied is the primary guard. If a row already
        // exists for this name in the default tenant (e.g. from an out-of-band
        // GraphQL mutation between migrations), skip it rather than failing.
        if agents_repo
            .get_by_name(&tenant.id, agent_name)
            .await
            .is_ok()
        {
            continue;
        }
        let capabilities = capabilities_to_strings(agent_cfg);
        // AgentTypeConfig.skills are REFERENCES to host-mounted skills
        // (S033 OverlayFs surface). They do NOT correspond to catalog skill
        // rows. Pass an empty skill_ids slice; per the spec, host skills and
        // catalog skills coexist via OverlayFs without round-tripping
        // host-only names through the catalog.
        let skill_ids: Vec<SkillId> = Vec::new();
        agents_repo
            .create(
                &tenant.id,
                NewAgent {
                    name: agent_name,
                    description: None,
                    system_prompt: agent_cfg.system_prompt.as_deref().unwrap_or(""),
                    model: &agent_cfg.model,
                    max_turns: agent_cfg.max_turns,
                    max_tokens: agent_cfg.max_tokens.map(|n| n as u32),
                    memory_pool_id: None,
                    skill_ids: &skill_ids,
                    capabilities: &capabilities,
                    channel_ids: &[],
                },
            )
            .await?;
        agents_imported += 1;
    }

    catalog.mark_seed_applied(SEED_SOURCE).await?;

    Ok(ImportOutcome::Imported {
        agents: agents_imported,
        skills: 0,
        memory_pools: pools_imported,
    })
}

/// Convert TOML capability config into the catalog's `Vec<String>` form.
///
/// Mirror of `simulacra_server::engine::agent_capabilities_from_config` — kept
/// local to avoid coupling simulacra-cli to simulacra-server. Shared by both
/// [`import_toml_seed`] (catalog rows) and [`fixtures_from_config`]
/// (in-memory `MemoryAgentRepository` rows) so the two surfaces produce
/// identical capability strings.
pub fn capabilities_to_strings(agent: &AgentTypeConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(caps) = agent.capabilities.as_ref() else {
        return out;
    };
    push_boolean_caps(caps, &mut out);
    for n in &caps.network {
        out.push(if n.starts_with("net:") {
            n.clone()
        } else {
            format!("net:{n}")
        });
    }
    for m in &caps.mcp {
        out.push(if m.starts_with("mcp:") {
            m.clone()
        } else {
            format!("mcp:{m}")
        });
    }
    out
}

fn push_boolean_caps(caps: &CapabilitiesConfig, out: &mut Vec<String>) {
    if caps.shell {
        out.push("shell:exec".into());
    }
    if caps.javascript {
        out.push("javascript".into());
    }
    if caps.python {
        out.push("python".into());
    }
}

// ---------------------------------------------------------------------------
// S042 Inc 3 Task 12 — `--no-catalog` mode helpers.
// ---------------------------------------------------------------------------

/// Stable namespace + id used for the implicit default tenant in both
/// catalog-backed (`get_or_create("default", ...)`) and `--no-catalog`
/// (`fixtures_from_config`) modes. Keeping the same string here lets tests
/// resolve agents by tenant without first round-tripping through a repo
/// lookup, and guarantees that any future code that joins on tenant id sees
/// the same value across modes.
pub const DEFAULT_TENANT_NAMESPACE: &str = "default";

/// Returns a `TenantId` whose string form equals
/// [`DEFAULT_TENANT_NAMESPACE`]. The CLI uses this as the implicit tenant for
/// every TOML-defined agent in `--no-catalog` mode.
pub fn default_tenant_id() -> TenantId {
    TenantId(DEFAULT_TENANT_NAMESPACE.to_owned())
}

/// Materialise `config` into an [`InMemoryFixtures`] bundle suitable for
/// constructing `MemoryAgentRepository`/`MemoryTenantRepository`/etc.
///
/// Layout:
/// - One tenant: `id = TenantId("default")`, namespace = `"default"`,
///   display_name = `Some("Default")`. Same identity used by the catalog
///   path's `get_or_create("default", Some("Default"))`.
/// - One memory pool named `"default"` belonging to the tenant **iff**
///   `config.memory.is_some()`. The pool's `config` JSON is the serialised
///   `MemoryConfig`; `embedding_model` is `None` (S038 `MemoryConfig`
///   carries no embedding model field today — same v1 reasoning as
///   [`import_toml_seed`]).
/// - One agent per `(name, AgentTypeConfig)` in `config.agent_types`. Fields
///   map 1:1 with `import_toml_seed`'s [`NewAgent`] — system_prompt defaults
///   to `""` when absent, max_turns is forwarded as-is (`None` → no cap),
///   max_tokens is widened to `Option<u32>` to match `Agent::max_tokens`,
///   memory_pool_id is `None` (TOML never associates an agent with a pool).
/// - Skills: empty (`AgentTypeConfig.skills` are name references to
///   host-mounted skills under S033 OverlayFs, not inline bodies — see the
///   v1 deferral note in [`import_toml_seed`]).
///
/// Capabilities are converted via [`capabilities_to_strings`] so the same
/// agent definition produces the same `Vec<String>` regardless of mode.
pub fn fixtures_from_config(config: &SimulacraConfig) -> SharedFixtures {
    let mut fx = InMemoryFixtures::default();
    let tenant_id = default_tenant_id();
    let now = Utc::now();

    fx.tenants.insert(
        tenant_id.clone(),
        Tenant {
            id: tenant_id.clone(),
            namespace: DEFAULT_TENANT_NAMESPACE.to_owned(),
            display_name: Some("Default".to_owned()),
            created_at: now,
            updated_at: now,
        },
    );

    if let Some(memory_cfg) = config.memory.as_ref() {
        let pool_id = MemoryPoolId::new();
        let pool_config = serde_json::to_value(memory_cfg).unwrap_or(Value::Null);
        fx.memory_pools.insert(
            pool_id.clone(),
            MemoryPool {
                id: pool_id,
                tenant_id: tenant_id.clone(),
                name: "default".to_owned(),
                // S038 `MemoryConfig` exposes no embedding-model field today;
                // mirror the catalog seed path so both modes leave this `None`.
                embedding_model: None,
                config: pool_config,
                created_at: now,
                updated_at: now,
            },
        );
    }

    for (agent_name, agent_cfg) in &config.agent_types {
        let agent_id = AgentId::new();
        let capabilities = capabilities_to_strings(agent_cfg);
        fx.agents.insert(
            agent_id.clone(),
            Agent {
                id: agent_id.clone(),
                tenant_id: tenant_id.clone(),
                name: agent_name.clone(),
                description: None,
                system_prompt: agent_cfg.system_prompt.clone().unwrap_or_default(),
                model: agent_cfg.model.clone(),
                max_turns: agent_cfg.max_turns.unwrap_or(0),
                max_tokens: agent_cfg.max_tokens.map(|n| n as u32),
                // TOML cannot associate an agent with a memory pool today —
                // mirrors `import_toml_seed`. The pool exists for parity but
                // is not joined.
                memory_pool_id: None,
                created_at: now,
                updated_at: now,
            },
        );
        // Empty skills (host references, not inline bodies — see doc comment).
        fx.agent_skills.insert(agent_id.clone(), Vec::new());
        fx.agent_capabilities.insert(agent_id, capabilities);
    }

    Arc::new(fx)
}

/// Bootstrap-time *intent* for the catalog plumbing.
///
/// Set synchronously by `bootstrap()` from `args.no_catalog` and stored on
/// `CliBootstrap` so tests can assert which path was selected without having
/// to actually open the DB. The async [`ensure_catalog`] helper consumes the
/// same `args`/`config` and *executes* the chosen plan, returning a richer
/// [`CatalogBootstrapResult`].
///
/// Two values are not the same struct because the planning step is sync and
/// the execution step is async — keeping them separate avoids cascading
/// `.await` through every existing sync `bootstrap()` caller.
#[derive(Debug, Clone)]
pub enum CatalogMode {
    /// Default. SQLite catalog at `db_path` will be opened on
    /// [`ensure_catalog`]; `import_toml_seed` will run against it.
    WithCatalog { db_path: PathBuf },
    /// `--no-catalog`. [`ensure_catalog`] will skip both Catalog::open and
    /// the seed import. Repositories must be constructed from the fixtures
    /// bundle returned by `ensure_catalog`.
    NoCatalog,
}

impl CatalogMode {
    /// True iff this is the `--no-catalog` variant.
    pub fn is_no_catalog(&self) -> bool {
        matches!(self, CatalogMode::NoCatalog)
    }

    /// The resolved catalog DB path, when this is the default-mode variant.
    /// Returns `None` for `--no-catalog`.
    pub fn db_path(&self) -> Option<&std::path::Path> {
        match self {
            CatalogMode::WithCatalog { db_path } => Some(db_path),
            CatalogMode::NoCatalog => None,
        }
    }
}

/// Outcome of [`ensure_catalog`] — the result of executing the planned
/// [`CatalogMode`].
#[derive(Debug)]
pub enum CatalogBootstrapResult {
    /// Default mode: catalog opened, `import_toml_seed` executed.
    WithCatalog {
        db_path: PathBuf,
        import_outcome: ImportOutcome,
    },
    /// `--no-catalog`: no SQLite file touched. `fixtures` is the in-memory
    /// bundle suitable for `MemoryAgentRepository::new(fixtures.clone())`.
    NoCatalog { fixtures: SharedFixtures },
}

impl CatalogBootstrapResult {
    /// True iff this is the `--no-catalog` variant.
    pub fn is_no_catalog(&self) -> bool {
        matches!(self, CatalogBootstrapResult::NoCatalog { .. })
    }

    /// The resolved catalog DB path, when this is the default-mode variant.
    pub fn db_path(&self) -> Option<&std::path::Path> {
        match self {
            CatalogBootstrapResult::WithCatalog { db_path, .. } => Some(db_path),
            CatalogBootstrapResult::NoCatalog { .. } => None,
        }
    }

    /// The fixtures bundle, when this is the `--no-catalog` variant.
    pub fn fixtures(&self) -> Option<&SharedFixtures> {
        match self {
            CatalogBootstrapResult::NoCatalog { fixtures } => Some(fixtures),
            CatalogBootstrapResult::WithCatalog { .. } => None,
        }
    }
}

/// Sync helper used by `bootstrap()` to compute [`CatalogMode`] without
/// touching disk. `ensure_catalog` consults `args.no_catalog` again later;
/// the two helpers must agree, so derive the plan from the same single field
/// here.
pub fn plan_catalog_mode(
    args: &CliArgs,
    config: &SimulacraConfig,
    state_dir: &std::path::Path,
) -> CatalogMode {
    if args.no_catalog {
        CatalogMode::NoCatalog
    } else {
        CatalogMode::WithCatalog {
            db_path: config.catalog.resolved_db_path(state_dir),
        }
    }
}

/// Errors returned by [`ensure_catalog`].
#[derive(Debug)]
pub enum EnsureCatalogError {
    /// `Catalog::open` failed in default mode. Operators selected the
    /// catalog path explicitly (no `--no-catalog`); we fail loudly rather
    /// than silently downgrade to in-memory mode.
    Open(CatalogError),
    /// `import_toml_seed` failed.
    Import(ImportError),
}

impl fmt::Display for EnsureCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnsureCatalogError::Open(e) => write!(f, "catalog open failed: {e}"),
            EnsureCatalogError::Import(e) => write!(f, "catalog import failed: {e}"),
        }
    }
}

impl std::error::Error for EnsureCatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EnsureCatalogError::Open(e) => Some(e),
            EnsureCatalogError::Import(e) => Some(e),
        }
    }
}

impl From<CatalogError> for EnsureCatalogError {
    fn from(e: CatalogError) -> Self {
        EnsureCatalogError::Open(e)
    }
}

impl From<ImportError> for EnsureCatalogError {
    fn from(e: ImportError) -> Self {
        EnsureCatalogError::Import(e)
    }
}

/// Branch the catalog bootstrap based on `args.no_catalog`.
///
/// Async because [`Catalog`] seed bookkeeping (`is_seed_applied`,
/// `mark_seed_applied`) and [`AgentRepository`] mutations all use
/// `tokio::task::spawn_blocking`. Kept separate from the sync `bootstrap()`
/// in `lib.rs` so existing sync test sites and the binary entrypoint don't
/// have to cascade `.await` through every caller. The binary glues the two
/// halves together inside the tokio runtime that `run_booted` already owns.
///
/// `state_dir` is the directory used to derive the default catalog DB path
/// when `[catalog].db_path` is unset. Passed in (rather than read from
/// `SimulacraConfig`) so callers can pin a temp directory in tests; the
/// production caller passes `PathBuf::from("./.simulacra")` per the v1 plan.
///
/// In `--no-catalog` mode this never touches the filesystem: it only
/// constructs the in-memory fixtures via [`fixtures_from_config`]. The
/// returned [`CatalogMode::NoCatalog`] carries the fixtures so that the
/// caller can build `MemoryAgentRepository`/etc. without re-running the
/// conversion.
pub async fn ensure_catalog(
    args: &CliArgs,
    config: &SimulacraConfig,
    state_dir: &std::path::Path,
) -> Result<CatalogBootstrapResult, EnsureCatalogError> {
    if args.no_catalog {
        // Spec line 561–563: no SQLite file opened or created; migrations
        // not invoked. Build fixtures so the in-memory repositories are
        // ready for use, but don't touch disk.
        return Ok(CatalogBootstrapResult::NoCatalog {
            fixtures: fixtures_from_config(config),
        });
    }

    let db_path = config.catalog.resolved_db_path(state_dir);
    let catalog = Catalog::open(&db_path)?;
    let import_outcome = import_toml_seed(&catalog, config).await?;
    Ok(CatalogBootstrapResult::WithCatalog {
        db_path,
        import_outcome,
    })
}
