pub mod activity_blocks;
pub mod catalog_import;
pub mod interactive;

pub use interactive::{
    HistoryDirection, InteractiveInput, InteractiveOutput, InteractiveSession,
    InteractiveSessionConfig, SessionView, StreamEvent, TerminalIo,
};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use std::path::PathBuf;
use tracing_subscriber::layer::SubscriberExt;

use rust_decimal::Decimal;
use simulacra_config::{
    AgentTypeConfig, CapabilitiesConfig, CatalogConfig, ProjectConfig, SimulacraConfig, TaskConfig,
    VfsConfig,
};
use simulacra_hooks::{HookPipeline, verdict::Operation};
use simulacra_memory::{
    BackgroundEmbedder, BackgroundEmbedderConfig, Chunker, ChunkerSelector, DefaultEmbedder,
    Embedder, HitIdCache, MarkdownSectionChunker, MemoryStore, RecentWritesBuffer,
    SqliteMemoryStore, SqliteVectorIndex, VectorIndex,
};
use simulacra_provider::{AnthropicProvider, OpenAiProvider};
use simulacra_runtime::{
    AgentLoop, AgentLoopConfig, CountingJournalStorage, InMemoryJournalStorage,
    SqliteJournalStorage,
};
use simulacra_sandbox::AgentCell;
use simulacra_tool::{
    MemoryToolHandles, SkillMeta, SkillTool, ToolRegistry, discover_and_filter_skills,
    register_memory_tools,
};
use simulacra_types::{
    AgentId, CapabilityToken, FsMetadata, JournalStorage, MemoryPath, Message, Provider,
    ResourceBudget, TenantId, ToolDefinition, VfsError, VfsSnapshot, VirtualFs,
};
use simulacra_vfs::{
    HookLister, IntegrationLister, MemoryFs, MemoryStoreFs, ProcFs, ProcState, ServiceFs,
    ToolLister, detect_project_root, process_host_mounts,
};

pub type CliError = anyhow::Error;

// Re-export ProviderKind publicly (it was pub in simulacra-cli before the move).
pub use simulacra_runtime::ProviderKind;
use simulacra_runtime::{AgentTaskFactory, DEFAULT_SYSTEM_PROMPT, SpawnAgentTool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliMode {
    Headless,
    Interactive,
}

#[derive(Debug, Clone, Parser)]
#[command(name = "simulacra", about = "AI agent framework")]
struct RawCliArgs {
    #[arg(long = "config", default_value = "simulacra.toml")]
    config_path: String,

    #[arg(long)]
    task: Option<String>,

    #[arg(long)]
    mode: Option<CliMode>,

    #[arg(long)]
    verbose: bool,

    #[arg(long = "otlp-endpoint")]
    otlp_endpoint: Option<String>,

    #[arg(long)]
    session: Option<String>,

    #[arg(long)]
    model: Option<String>,

    /// Maximum number of agent turns per prompt (0 = unlimited)
    #[arg(long)]
    max_turns: Option<u32>,

    /// Maximum token budget (0 = unlimited)
    #[arg(long)]
    max_tokens: Option<u64>,

    /// Maximum cost budget in USD (0 = unlimited) [reserved for future use]
    #[arg(long)]
    max_cost: Option<f64>,

    /// S042 Inc 3 Task 12: Skip the SQLite catalog; resolve agents from
    /// `simulacra.toml` only. No DB file is created or read; mutating catalog
    /// repository methods return `CatalogError::ReadOnly`.
    #[arg(
        long,
        help = "Skip the SQLite catalog; resolve agents from simulacra.toml only"
    )]
    no_catalog: bool,
}

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub config_path: String,
    pub task: Option<String>,
    pub mode: Option<CliMode>,
    pub verbose: bool,
    pub otlp_endpoint: Option<String>,
    pub session: Option<String>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_cost: Option<f64>,
    /// S042 Inc 3 Task 12: when `true`, the CLI bypasses the SQLite catalog
    /// entirely. See [`ensure_catalog`] and [`CliArgs::no_catalog`] CLI help.
    pub no_catalog: bool,
}

impl CliArgs {
    fn from_raw(raw: RawCliArgs) -> Self {
        let mode = raw.mode.or_else(|| {
            if raw.task.is_some() {
                Some(CliMode::Headless)
            } else {
                None
            }
        });
        Self {
            config_path: raw.config_path,
            task: raw.task,
            mode,
            verbose: raw.verbose,
            otlp_endpoint: raw.otlp_endpoint,
            session: raw.session,
            model: raw.model,
            max_turns: raw.max_turns,
            max_tokens: raw.max_tokens,
            max_cost: raw.max_cost,
            no_catalog: raw.no_catalog,
        }
    }
}

impl CommandFactory for CliArgs {
    fn command() -> clap::Command {
        RawCliArgs::command()
    }

    fn command_for_update() -> clap::Command {
        RawCliArgs::command_for_update()
    }
}

impl FromArgMatches for CliArgs {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let raw = RawCliArgs::from_arg_matches(matches)?;
        Ok(Self::from_raw(raw))
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        let mut raw = RawCliArgs {
            config_path: self.config_path.clone(),
            task: self.task.clone(),
            mode: self.mode,
            verbose: self.verbose,
            otlp_endpoint: self.otlp_endpoint.clone(),
            session: self.session.clone(),
            model: self.model.clone(),
            max_turns: self.max_turns,
            max_tokens: self.max_tokens,
            max_cost: self.max_cost,
            no_catalog: self.no_catalog,
        };
        raw.update_from_arg_matches(matches)?;
        *self = Self::from_raw(raw);
        Ok(())
    }
}

impl Parser for CliArgs {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TracingBackend {
    StderrFmt,
    Otlp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TracingPlan {
    pub backend: TracingBackend,
    pub level: String,
    pub otlp_endpoint: Option<String>,
}

pub struct CliBootstrap {
    pub config: SimulacraConfig,
    pub mode: CliMode,
    pub task: String,
    pub entry_agent: String,
    pub model: String,
    pub capability_token: CapabilityToken,
    pub resource_budget: ResourceBudget,
    pub vfs: Arc<dyn VirtualFs>,
    pub tool_definitions: Vec<ToolDefinition>,
    pub provider_kind: ProviderKind,
    pub tracing_plan: TracingPlan,
    // Internal: keep refs for building AgentLoop
    tool_registry: ToolRegistry,
    journal: Arc<dyn JournalStorage>,
    #[allow(dead_code)]
    budget_arc: Arc<Mutex<ResourceBudget>>,
    #[allow(dead_code)]
    proc_turn: Arc<std::sync::atomic::AtomicU64>,
    /// Receiver for spawn_agent messages, created when can_spawn is non-empty.
    /// Passed to the supervisor's run_actor_loop.
    #[allow(dead_code)]
    spawn_rx: Option<tokio::sync::mpsc::Receiver<simulacra_runtime::SupervisorMessage>>,
    /// Sender clone for AgentTaskFactory so children can spawn descendants (S018 §173).
    spawn_tx: Option<tokio::sync::mpsc::Sender<simulacra_runtime::SupervisorMessage>>,
    /// S019: Activity sink for emitting events. Created at bootstrap time so
    /// SpawnAgentTool and AgentLoop share the same sink.
    activity_sink: Option<Arc<dyn simulacra_runtime::ActivitySink>>,
    /// S019: Receiver end of the activity event channel (interactive mode only).
    activity_rx: Option<tokio::sync::mpsc::UnboundedReceiver<simulacra_types::ActivityEvent>>,
    /// S017: Discovered and filtered skill catalog for the entry agent.
    pub skill_catalog: Vec<SkillMeta>,
    /// S020: Project root directory (parent of resolved config path).
    pub project_root: PathBuf,
    /// S026: Governance hook pipeline.
    pub(crate) pipeline: Arc<HookPipeline>,
    /// S038: Memory subsystem state to be consumed by `run_booted` once the
    /// tokio runtime exists. `None` when memory is not wired (either no
    /// `[memory]` section or the entry agent has memory disabled).
    pub(crate) memory_runtime: Option<MemoryRuntimeState>,
    /// S038: Telemetry payload for the `memory_bootstrap` span emitted in
    /// `run_booted`. `None` when no `[memory]` section was configured.
    pub(crate) memory_bootstrap_info: Option<MemoryBootstrapInfo>,
    /// S033: Integration registry deferred to `run_booted` so that
    /// `start_background_refresh` is called inside the real tokio runtime
    /// (not a temporary one), preventing task orphaning and OTel state corruption.
    integration_registry_for_refresh: Option<Arc<simulacra_integration::IntegrationRegistry>>,
    /// S042 Inc 3 Task 12: planning-time catalog mode derived from
    /// `CliArgs::no_catalog`. Surfaces *which* path will be taken without
    /// requiring tests to actually open the DB. The matching execution
    /// happens via [`catalog_import::ensure_catalog`].
    pub catalog_mode: catalog_import::CatalogMode,
    /// S042 Inc 3 Task 12: in-memory fixtures bundle, populated synchronously
    /// during `bootstrap()` when `--no-catalog` is set. `None` in default
    /// mode (the catalog itself is the source of truth there). Exposed so
    /// tests can construct `MemoryAgentRepository`/etc. and assert that the
    /// CLI's `SimulacraConfig` is faithfully materialised. The v1 CLI does not
    /// yet consume this internally — see Task 12 honest-scoping note.
    pub fixtures: Option<simulacra_catalog::repo::memory::SharedFixtures>,
}

impl CliBootstrap {
    /// Hook names registered for the given operation in the bootstrapped
    /// pipeline. Surface for tests + diagnostics; the runtime consumes the
    /// pipeline directly via the internal field.
    ///
    /// `operation` accepts the snake_case forms emitted by
    /// `simulacra_hooks::Operation::Display`: `tool_call`, `llm`, `spawn`,
    /// `http_request`, and `vfs_write` (S039). Unknown values yield an empty
    /// vec.
    pub fn hook_names(&self, operation: &str) -> Vec<String> {
        use simulacra_hooks::verdict::Operation;
        let op = match operation {
            "tool_call" => Operation::ToolCall,
            "llm" => Operation::Llm,
            "spawn" => Operation::Spawn,
            "http_request" => Operation::HttpRequest,
            "vfs_write" => Operation::VfsWrite,
            _ => return vec![],
        };
        self.pipeline.hook_names(op)
    }
}

/// S038: Handles needed by `run_booted` to spawn the `BackgroundEmbedder`
/// after the tokio runtime is created. Local to `simulacra-cli`.
///
/// Note: the `RecentWritesBuffer` Arc is NOT stashed here — it's cloned
/// into `MemoryStoreFs` and `MemoryToolHandles` at bootstrap time, which
/// is the full consumption. `run_booted` needs only the handles the
/// `BackgroundEmbedder::spawn` call requires.
/// Convert the TOML-parsed retention config into the runtime reaper config.
/// Prefixes are parsed as `MemoryPath`; invalid prefixes are dropped with a
/// warning so a malformed retention subtree does not prevent bootstrap
/// (the rest of the memory subsystem still works).
fn retention_config_to_reaper(
    cfg: &simulacra_config::MemoryRetentionConfig,
) -> simulacra_memory::RetentionReaperConfig {
    use std::time::Duration;
    let subtrees = cfg
        .subtrees
        .iter()
        .filter_map(
            |sub| match simulacra_types::MemoryPath::parse(&sub.prefix) {
                Ok(prefix) => Some(simulacra_memory::RetentionSubtree {
                    prefix,
                    ttl: Duration::from_secs(sub.ttl_secs),
                }),
                Err(e) => {
                    tracing::warn!(
                        prefix = %sub.prefix,
                        error = %e,
                        "retention: skipping subtree with invalid prefix"
                    );
                    None
                }
            },
        )
        .collect();
    simulacra_memory::RetentionReaperConfig {
        interval: Duration::from_secs(cfg.interval_secs),
        batch_size: cfg.batch_size,
        subtrees,
    }
}

pub(crate) struct MemoryRuntimeState {
    pub(crate) tenant: TenantId,
    pub(crate) store: Arc<dyn MemoryStore>,
    pub(crate) index: Arc<dyn VectorIndex>,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) chunker_selector: ChunkerSelector,
    /// S037 §20 Retention: converted from `[memory.retention]` config when
    /// present. `None` means no reaper runs.
    pub(crate) retention: Option<simulacra_memory::RetentionReaperConfig>,
}

/// S038: Telemetry attributes captured during sync preflight and emitted as
/// the `memory_bootstrap` span in `run_booted` (so it can be a child of
/// `cli_run`).
#[derive(Debug, Clone)]
pub(crate) struct MemoryBootstrapInfo {
    pub(crate) dir: String,
    pub(crate) tenant: String,
    pub(crate) embedder_id: String,
    pub(crate) embedder_dim: usize,
    pub(crate) entry_agent_enabled: bool,
    pub(crate) outcome: &'static str,
}

/// S038: A no-op VFS layer that intercepts memory paths (`/var/memory/**`,
/// `/mnt/**`) and returns `NotFound` for every operation. Installed when
/// memory is not wired (either no `[memory]` section or the entry agent has
/// memory disabled), so memory writes do not silently succeed against the
/// inner `MemoryFs`.
struct MemoryRejectFs<V: VirtualFs> {
    inner: V,
}

impl<V: VirtualFs> MemoryRejectFs<V> {
    fn new(inner: V) -> Self {
        Self { inner }
    }

    fn is_memory(path: &str) -> bool {
        MemoryPath::is_memory_path_str(path)
    }
}

impl<V: VirtualFs> VirtualFs for MemoryRejectFs<V> {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        if Self::is_memory(path) {
            return Err(VfsError::NotFound(path.to_string()));
        }
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        if Self::is_memory(path) {
            return Err(VfsError::NotFound(path.to_string()));
        }
        self.inner.write(path, data)
    }
    fn exists(&self, path: &str) -> bool {
        if Self::is_memory(path) {
            return false;
        }
        self.inner.exists(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        if Self::is_memory(path) {
            return Err(VfsError::NotFound(path.to_string()));
        }
        self.inner.list_dir(path)
    }
    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        if Self::is_memory(path) {
            return Err(VfsError::NotFound(path.to_string()));
        }
        self.inner.mkdir(path)
    }
    fn remove(&self, path: &str) -> Result<(), VfsError> {
        if Self::is_memory(path) {
            return Err(VfsError::NotFound(path.to_string()));
        }
        self.inner.remove(path)
    }
    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        if Self::is_memory(path) {
            return Err(VfsError::NotFound(path.to_string()));
        }
        self.inner.metadata(path)
    }
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.inner.snapshot()
    }
    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.inner.restore(snapshot)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOutput {
    pub stdout_content: String,
    pub stderr_content: String,
    pub exit_code: i32,
    pub telemetry_flushed: bool,
}

// ---------------------------------------------------------------------------
// ProviderWrapper — wraps Box<dyn Provider> to implement Provider
// ---------------------------------------------------------------------------

/// Wrapper that delegates Provider to an inner Box<dyn Provider>.
/// Needed because InteractiveSession is generic over P: Provider (which requires Sized),
/// but run_with_provider receives a Box<dyn Provider>.
#[allow(dead_code)]
struct ProviderWrapper(Mutex<Option<Box<dyn Provider>>>);

impl Provider for ProviderWrapper {
    fn chat<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a [ToolDefinition],
        _budget: &'a mut ResourceBudget,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        simulacra_types::ProviderResponse,
                        simulacra_types::ProviderError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        // This provider is never called — interactive mode uses AgentLoop directly.
        Box::pin(async {
            Err(simulacra_types::ProviderError::Other(
                "ProviderWrapper: not intended to be called directly".into(),
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// S017: Skill discovery
// ---------------------------------------------------------------------------

/// Discover skills from the VFS and filter them by agent_type.skills and
/// capability token to produce the effective skill catalog.
///
/// Skills are discovered from project VFS `/skills/*/SKILL.md` paths and
/// configured host skill mounts (which are mounted into the VFS before
/// discovery).
///
/// If an agent type references a skill name that is not discovered
/// successfully, startup fails with an error naming the agent type and
/// missing skill.
///
/// The `agent_type.skills = ["..."]` restricts the skills exposed to that
/// agent.
///
/// A child agent's available skills come from the child agent type's `skills`
/// list, not from the parent's currently loaded skill bodies. Loaded parent
/// skill bodies are not copied automatically into child conversations. If a
/// child needs a skill, it must resolve that skill in its own context through
/// its own effective skill catalog.
///
/// Model-triggered `Skill` calls are journaled and observed exactly like
/// other tool calls (JournalEntryKind::ToolCall and JournalEntryKind::ToolResult
/// with the "Skill" tool name).
///
pub fn infer_provider_kind(model: &str) -> Result<ProviderKind> {
    if model.starts_with("claude-") {
        Ok(ProviderKind::Anthropic)
    } else if model.starts_with("ollama:") {
        Ok(ProviderKind::Ollama)
    } else {
        // Everything else goes through the OpenAI-compatible endpoint.
        // This covers gpt-*, o1-*, o3-*, and also Groq/Together/OpenRouter
        // model names like llama-3.3-70b-versatile, deepseek-r1, etc.
        Ok(ProviderKind::OpenAI)
    }
}

pub fn bootstrap(args: &CliArgs) -> Result<CliBootstrap> {
    // 1. Tracing plan
    let level = if args.verbose { "DEBUG" } else { "INFO" };
    let backend = if args.otlp_endpoint.is_some() {
        TracingBackend::Otlp
    } else {
        TracingBackend::StderrFmt
    };
    let tracing_plan = TracingPlan {
        backend,
        level: level.to_string(),
        otlp_endpoint: args.otlp_endpoint.clone(),
    };

    // 2. Determine mode
    let mode = match args.mode {
        Some(m) => m,
        None => {
            if args.task.is_some() {
                CliMode::Headless
            } else {
                bail!("no mode specified and no --task provided");
            }
        }
    };

    // 2b. Validate the --session value (if any) before it is used as a
    // filesystem path component. See `validate_session_id` for details.
    if let Some(ref s) = args.session {
        validate_session_id(s)?;
    }

    // 3. Load config
    let config = load_config(args, mode)?;

    // 4. Resolve task: args.task > config.task.task (optional in interactive mode)
    let task = args
        .task
        .clone()
        .or_else(|| config.task.as_ref().and_then(|t| t.task.clone()));
    if task.is_none() && mode != CliMode::Interactive {
        bail!("no task specified. Use --task or set [task].task in config.");
    }
    let task = task.unwrap_or_default();

    // 6. Resolve entry_agent
    let entry_agent = config
        .task
        .as_ref()
        .map(|t| t.entry_agent.clone())
        .unwrap_or_else(|| "default".to_string());

    // 7. Resolve model: --model flag > agent type config
    let agent_type = config
        .agent_types
        .get(&entry_agent)
        .ok_or_else(|| anyhow!("agent type {entry_agent:?} not found in config"))?;
    let model = args
        .model
        .clone()
        .unwrap_or_else(|| agent_type.model.clone());

    // 8. Build CapabilityToken
    let capability_token = simulacra_config::build_capability_token(agent_type);

    // 9. Build ResourceBudget (CLI flags override config)
    let max_turns = args
        .max_turns
        .unwrap_or_else(|| agent_type.max_turns.unwrap_or(50));
    let max_tokens = args
        .max_tokens
        .unwrap_or_else(|| agent_type.max_tokens.unwrap_or(200_000));
    // Respect the agent type's configured `max_sub_agents`. If the field is
    // absent we fall back to a conservative default (10) rather than the
    // previous hard-coded `0`, which in the budget enforcement layer means
    // "unlimited" and silently drops any configured limit.
    let max_sub_agents = agent_type.max_sub_agents.unwrap_or(10);
    let resource_budget = ResourceBudget::new(max_tokens, max_turns, Decimal::ZERO, max_sub_agents);

    // 10. Infer provider kind
    let provider_kind = infer_provider_kind(&model)?;

    // 11. Create VFS
    //
    // ProcFs is the outermost layer (added after pipeline is built below).
    // SharedToolList is populated after the full registry is built.
    let shared_tools = SharedToolList::default();
    let proc_turn = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let proc_journal_entries = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let inner_vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
    let _ = inner_vfs.mkdir("/workspace");

    // S020: Detect project root and process host mounts
    let is_adhoc = load_config_result_is_adhoc(args);
    let project_root = detect_project_root(&args.config_path, is_adhoc)?;

    if !is_adhoc {
        process_host_mounts(&inner_vfs, &config, &project_root, &entry_agent)?;
    }

    // Pre-seed /workspace/task.md AFTER mounts (so it overwrites any mounted task.md)
    if !task.is_empty() {
        inner_vfs
            .write("/workspace/task.md", task.as_bytes())
            .context("failed to pre-seed /workspace/task.md")?;
    }

    // S038: Memory subsystem sync preflight.
    //
    // Builds the SQLite store + vector index + embedder when [memory] is
    // present. The runtime state (BackgroundEmbedder spawn args) is stashed
    // on `CliBootstrap` for `run_booted` to consume after the tokio runtime
    // is created. The VFS stack is wrapped with either:
    //   - `MemoryStoreFs` when [memory] is configured AND the entry agent's
    //     `MemoryCapability` is enabled (memory routes to durable store)
    //   - `MemoryRejectFs` otherwise (memory paths return NotFound, no
    //     silent success against the inner MemoryFs)
    let entry_agent_memory_enabled = capability_token.memory.enabled;
    let mut memory_runtime: Option<MemoryRuntimeState> = None;
    let mut memory_bootstrap_info: Option<MemoryBootstrapInfo> = None;
    let mut memory_tool_handles: Option<MemoryToolHandles> = None;

    let post_memory_vfs: Arc<dyn VirtualFs> = if let Some(ref memory_cfg) = config.memory {
        let tenant = TenantId::parse(&memory_cfg.tenant)
            .map_err(|e| anyhow!("memory: invalid tenant id: {e}"))?;
        std::fs::create_dir_all(&memory_cfg.dir)
            .map_err(|e| anyhow!("memory: cannot create memory dir: {e}"))?;
        let embedder_concrete = DefaultEmbedder::load_default().map_err(|e| {
            simulacra_memory::record_embedder_load_failure("load_default");
            anyhow!("memory: load embedder failed: {e}")
        })?;
        let embedder_id_str = embedder_concrete.id().to_string();
        let embedder_dim = embedder_concrete.dim();
        let embedder: Arc<dyn Embedder> = Arc::new(embedder_concrete);

        // S037 §13: apply the configured on_model_change policy before
        // constructing the reconciled index. On mismatch, this either
        // surfaces an error (Refuse), stages a reindex_background
        // backlog with the old→new embedder flip, or wipes+rebuilds at
        // the new dim. Fresh tenants are a no-op.
        let policy = match memory_cfg.on_model_change {
            simulacra_config::OnModelChange::Refuse => {
                simulacra_memory::OnModelChangePolicy::Refuse
            }
            simulacra_config::OnModelChange::ReindexBackground => {
                simulacra_memory::OnModelChangePolicy::ReindexBackground
            }
            simulacra_config::OnModelChange::WipeAndRebuild => {
                simulacra_memory::OnModelChangePolicy::WipeAndRebuild
            }
        };
        simulacra_memory::apply_policy(&memory_cfg.dir, &tenant, embedder.id(), policy)
            .map_err(|e| anyhow!("memory: on_model_change policy failed: {e}"))?;

        let store_concrete = SqliteMemoryStore::new(&memory_cfg.dir)
            .map_err(|e| anyhow!("memory: memory store open failed: {e}"))?;
        let store: Arc<dyn MemoryStore> = Arc::new(store_concrete);
        let index_concrete = SqliteVectorIndex::new(&memory_cfg.dir, embedder.id().clone())
            .map_err(|e| anyhow!("memory: vector index open failed: {e}"))?;
        let index: Arc<dyn VectorIndex> = Arc::new(index_concrete);
        store
            .ensure_tenant(&tenant)
            .map_err(|e| anyhow!("memory: ensure_tenant failed: {e}"))?;
        index
            .ensure_tenant(&tenant)
            .map_err(|e| anyhow!("memory: ensure_tenant failed: {e}"))?;
        let rrwb = Arc::new(Mutex::new(RecentWritesBuffer::new()));
        let chunker_selector: ChunkerSelector = {
            let md = Arc::new(MarkdownSectionChunker) as Arc<dyn Chunker>;
            Arc::new(move |path: &MemoryPath| {
                if path.as_str().ends_with(".md") {
                    Some(md.clone())
                } else {
                    None
                }
            })
        };

        let dir_str = memory_cfg.dir.display().to_string();
        let tenant_str = memory_cfg.tenant.clone();

        let wrapped: Arc<dyn VirtualFs> = if entry_agent_memory_enabled {
            // Wrap inner VFS with MemoryStoreFs (gates memory paths via the
            // entry agent's MemoryCapability) and stash the per-run RRWB so
            // both the FS layer and the memory tools share the same Arc.
            let mem_fs = MemoryStoreFs::new(
                inner_vfs,
                tenant.clone(),
                Arc::clone(&store),
                capability_token.memory.clone(),
            )
            .with_rrwb(Arc::clone(&rrwb));

            memory_tool_handles = Some(MemoryToolHandles {
                tenant: tenant.clone(),
                capability: capability_token.memory.clone(),
                memory_store: Arc::clone(&store),
                vector_index: Arc::clone(&index),
                embedder: Arc::clone(&embedder),
                hit_cache: Arc::new(HitIdCache::new()),
                rrwb: Some(Arc::clone(&rrwb)),
                // Pipeline is built further down; attach after construction.
                hook_pipeline: None,
            });

            // S037 §20: translate `[memory.retention]` config into a reaper
            // config. `None` means no reaper will be spawned in run_booted.
            let retention_cfg = memory_cfg
                .retention
                .as_ref()
                .map(retention_config_to_reaper);

            memory_runtime = Some(MemoryRuntimeState {
                tenant: tenant.clone(),
                store: Arc::clone(&store),
                index: Arc::clone(&index),
                embedder: Arc::clone(&embedder),
                chunker_selector,
                retention: retention_cfg,
            });

            memory_bootstrap_info = Some(MemoryBootstrapInfo {
                dir: dir_str,
                tenant: tenant_str,
                embedder_id: embedder_id_str,
                embedder_dim,
                entry_agent_enabled: true,
                outcome: "wired",
            });

            Arc::new(mem_fs)
        } else {
            tracing::warn!(
                "memory is configured in simulacra.toml but the entry agent does not use it"
            );
            memory_bootstrap_info = Some(MemoryBootstrapInfo {
                dir: dir_str,
                tenant: tenant_str,
                embedder_id: embedder_id_str,
                embedder_dim,
                entry_agent_enabled: false,
                outcome: "skipped_disabled_for_entry_agent",
            });
            Arc::new(MemoryRejectFs::new(inner_vfs))
        };
        wrapped
    } else {
        if entry_agent_memory_enabled {
            tracing::warn!(
                entry_agent = %entry_agent,
                "memory enabled in agent type {entry_agent} but no [memory] section in simulacra.toml; agent will have no memory tools"
            );
        }
        Arc::new(MemoryRejectFs::new(inner_vfs))
    };
    let inner_vfs = post_memory_vfs;

    // 12. Create AgentCell and register tools
    let inner_journal: Arc<dyn JournalStorage> = if let Some(home) = std::env::var_os("HOME") {
        let journals_dir = std::path::PathBuf::from(home).join(".simulacra/journals");
        if let Err(e) = std::fs::create_dir_all(&journals_dir) {
            tracing::warn!("failed to create journals dir, falling back to in-memory: {e}");
            Arc::new(InMemoryJournalStorage::new())
        } else {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let db_path = journals_dir.join(format!("{ts}-{}.db", std::process::id()));
            match SqliteJournalStorage::new(db_path) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    tracing::warn!("failed to open SQLite journal, falling back to in-memory: {e}");
                    Arc::new(InMemoryJournalStorage::new())
                }
            }
        }
    } else {
        Arc::new(InMemoryJournalStorage::new())
    };
    let journal: Arc<dyn JournalStorage> = Arc::new(CountingJournalStorage::new(
        inner_journal,
        Arc::clone(&proc_journal_entries),
    ));
    let budget_arc = Arc::new(Mutex::new(resource_budget.clone()));

    // S026 + S039: Build governance hook pipeline from config. `vfs_write`
    // chains are loaded the same way as the existing op chains so a
    // `[[hooks.vfs_write]]` entry in `simulacra.toml` is reachable from the
    // bootstrapped pipeline.
    let pipeline = Arc::new({
        let mut p = HookPipeline::new();
        if let Some(ref hooks) = config.hooks {
            for (op, entries) in [
                (Operation::ToolCall, &hooks.tool_call),
                (Operation::Llm, &hooks.llm),
                (Operation::Spawn, &hooks.spawn),
                (Operation::HttpRequest, &hooks.http_request),
                (Operation::VfsWrite, &hooks.vfs_write),
            ] {
                for entry in entries {
                    if entry.runtime == "js" {
                        match simulacra_hooks::js::JsHookModule::from_file(
                            &entry.name,
                            &entry.module,
                            entry.timeout_ms,
                        ) {
                            Ok(h) => {
                                p.add(op, Arc::new(h));
                                tracing::info!(hook = %entry.name, operation = %op, "hook registered");
                            }
                            Err(e) => {
                                tracing::warn!(hook = %entry.name, error = %e, "failed to load hook");
                            }
                        }
                    }
                }
            }
        }
        p
    });

    // Backfill the hook pipeline into memory_tool_handles now that it exists.
    // See S037 §20: memory tools must consult the governance pipeline for
    // before/after `tool_call` hooks with graceful-deny shapes.
    if let Some(ref mut h) = memory_tool_handles {
        h.hook_pipeline = Some(Arc::clone(&pipeline));
    }

    // S033: Build IntegrationRegistry from config (if any integrations configured).
    let integration_registry = if !config.integrations.is_empty() {
        match simulacra_integration::IntegrationRegistry::from_config(&config.integrations) {
            Ok(r) => {
                tracing::info!(
                    integration_count = config.integrations.len(),
                    "integration registry initialized"
                );
                Some(Arc::new(r))
            }
            Err(e) => {
                tracing::warn!(error = %e, "integration registry failed — continuing without integrations");
                None
            }
        }
    } else {
        None
    };

    let integration_lister: Arc<dyn IntegrationLister> = Arc::new(RegistryIntegrationLister {
        registry: integration_registry.clone(),
    });

    // S029 + S033: Wrap inner_vfs with ServiceFs then ProcFs.
    // ServiceFs intercepts /svc/**, ProcFs intercepts /proc/**, both delegate the rest.
    let with_svc = ServiceFs::new(inner_vfs, integration_lister);
    let vfs: Arc<dyn VirtualFs> = Arc::new(ProcFs::new(
        with_svc,
        Arc::new(ProcState {
            agent_id: format!("{}", std::process::id()),
            agent_name: entry_agent.to_string(),
            model: model.clone(),
            parent_id: None,
            budget: Arc::clone(&budget_arc),
            capabilities: capability_token.clone(),
            tools: Arc::new(shared_tools.clone()),
            session_id: uuid::Uuid::new_v4().to_string(),
            session_start: std::time::Instant::now(),
            journal_entries: Arc::clone(&proc_journal_entries),
            hooks: Arc::new(PipelineHookLister(Arc::clone(&pipeline))),
            turn: Arc::clone(&proc_turn),
        }),
    ));

    let http_client: Arc<dyn simulacra_http::HttpClient> = Arc::new(
        simulacra_http::UreqHttpClient::with_pipeline(30_000, 10, Some(Arc::clone(&pipeline))),
    );
    let mut cell = AgentCell::new(
        Arc::clone(&vfs),
        capability_token.clone(),
        Arc::clone(&budget_arc),
        Arc::clone(&journal),
        http_client,
    );
    cell.set_script_executor(simulacra_sandbox::ScriptExecutor::new(4));

    // S033: Wire integration registry into AgentCell for credential injection.
    if let Some(ref reg) = integration_registry {
        cell.integration_registry = Some(Arc::clone(reg));
        // Deny-by-default: only integrations explicitly granted to the
        // current tenant are injected. In CLI mode we resolve the "current
        // tenant" as the first tenant entry whose `agent_type` matches the
        // resolved entry agent. If no such tenant is configured, no
        // integrations are granted. This prevents the CLI agent from
        // silently inheriting every configured integration's credentials.
        cell.tenant_integrations = config
            .tenants
            .values()
            .find(|t| t.agent_type == entry_agent)
            .and_then(|t| t.integrations.clone())
            .unwrap_or_default();
    }

    let cell = Arc::new(cell);

    let mut registry = ToolRegistry::new();
    registry.set_pipeline(Arc::clone(&pipeline));
    simulacra_tool::register_builtins(&mut registry, Arc::clone(&cell));

    // S038: Register memory tools when the entry agent has memory enabled
    // AND a [memory] section is present (handles set during the sync
    // preflight above).
    if let Some(handles) = memory_tool_handles.take() {
        register_memory_tools(&mut registry, handles);
    }

    // S017: Discover skills and conditionally register the Skill tool.
    //
    // Skills are discovered from two sources at bootstrap:
    //   - project-local VFS paths under /skills/<dir>/SKILL.md
    //   - configured host skill paths that are mounted read-only into the VFS
    //     before discovery
    //
    // Configured host skill roots are mounted into the VFS at bootstrap time
    // before discovery. After mounting, the rest of the system resolves them
    // exactly like project skills.
    //
    // Each discovered SKILL.md is parsed once at bootstrap to extract frontmatter
    // metadata and its canonical VFS path. The markdown body is NOT retained in
    // the initial prompt state.
    //
    // The skill registry is keyed by frontmatter `name`, not directory name.
    // Duplicate skill names across discovery sources are a startup error.
    // A discovered directory with missing or invalid SKILL.md frontmatter is
    // skipped with a warning (warn!) unless an agent type explicitly references
    // that skill name; referenced invalid skills are a startup error.
    //
    // The effective skill catalog is the intersection of:
    //   - the agent type's configured skills list (agent_type.skills)
    //   - the discovered skill registry
    //   - the capability token's allowed skill:<name> patterns
    // This filter produces the per-agent effective skill catalog.
    //
    // Relative resources referenced by a skill are resolved relative to the
    // skill directory containing that SKILL.md.
    let skill_catalog =
        discover_and_filter_skills(&vfs, &agent_type.skills, &capability_token, &entry_agent)?;

    // Agents with at least one model-visible skill register exactly one
    // built-in tool named `Skill`. Simulacra does NOT register one tool per skill.
    // Skills are not first-class tools.
    //
    // Agents with only user-invocable or model-disabled skills do not register
    // the Skill tool for that agent. do not expose an empty Skill tool definition
    // to the model.
    //
    // User-triggered skill resolution in interactive mode still works for any
    // remaining user_invocable skills.
    //
    // The Skill tool definition is built from the current agent's effective
    // skill catalog after agent-type config and capability filtering are applied.
    // The definition includes only name + description metadata. Full SKILL.md
    // bodies are excluded.
    //
    // The metadata budget for skill descriptions is derived as a configured
    // percentage of the active model's context window. Only model-invocable
    // skills count against the metadata budget. Metadata entries are considered
    // in agent_type.skills order. Simulacra includes as many name + description
    // entries as fit within the metadata budget and omits the remainder from
    // the model-visible Skill tool definition.
    //
    // If one or more model-invocable skills are omitted due to the metadata
    // budget, the Skill tool description MUST indicate that the catalog is partial.
    //
    // Omitted skills remain user-invocable when policy allows.
    let has_model_visible = skill_catalog.iter().any(|s| !s.disable_model_invocation);
    if has_model_visible {
        registry.register(Box::new(SkillTool::new(
            Arc::clone(&cell),
            skill_catalog.clone(),
        )));
    }

    // Register MCP tools from config.
    //
    // MCP servers configured in [mcp.servers] are connected and their tools
    // discovered at bootstrap time. Each MCP tool is wrapped as a Simulacra Tool
    // and registered in the ToolRegistry so the LLM can invoke it.
    //
    // Connection failures are logged and skipped — they do not prevent startup.
    // The MCP handshake requires async I/O, so we create a temporary tokio
    // runtime for this one-time startup cost.
    if let Some(ref mcp_config) = config.mcp {
        // Partition into network-transport servers (HTTP/SSE/auto) and
        // WASM-transport servers. WASM modules need a different
        // bootstrap path (compile + load + connect_wasm_module) but
        // share the underlying McpManager so all tools — wherever they
        // come from — go through the same capability + routing layer.
        let network_descriptors: Vec<(String, Option<String>, Option<String>)> = mcp_config
            .servers
            .iter()
            .filter(|s| s.transport.as_deref() != Some("wasm"))
            .map(|s| (s.name.clone(), s.url.clone(), s.transport.clone()))
            .collect();

        // Each WASM MCP server gets the agent's journal and governance
        // hook pipeline so `simulacra:mcp/http.fetch` invocations from the
        // module run through the same audit trail and policy chain as
        // host-side fetches. agent_id is empty here because the
        // McpManager is shared across the CLI process; per-agent
        // attribution is a future spec.
        let wasm_descriptors: Vec<simulacra_mcp::WasmMcpServerDescriptor> = mcp_config
            .servers
            .iter()
            .filter(|s| s.transport.as_deref() == Some("wasm"))
            .filter_map(|s| {
                let module = s.module.as_ref()?;
                Some(simulacra_mcp::WasmMcpServerDescriptor {
                    name: s.name.clone(),
                    module_path: std::path::PathBuf::from(module),
                    network_allowlist: s.network.clone(),
                    hooks: Some(Arc::clone(&pipeline)),
                    journal: Some(Arc::clone(&journal)),
                    agent_id: simulacra_types::AgentId(String::new()),
                })
            })
            .collect();

        let total_servers = network_descriptors.len() + wasm_descriptors.len();

        if total_servers > 0 {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to build tokio runtime for MCP bootstrap")?;

            let mcp_tools = rt.block_on(simulacra_mcp::create_mcp_tools_with_wasm(
                &network_descriptors,
                &wasm_descriptors,
            ));

            tracing::info!(
                mcp_tool_count = mcp_tools.len(),
                mcp_server_count = total_servers,
                "MCP tool discovery complete"
            );

            for tool in mcp_tools {
                registry.register(Box::new(tool));
            }
        }
    }

    // Register WASM tools from config (feature-gated).
    #[cfg(feature = "wasm")]
    if let Some(ref wasm_config) = config.wasm {
        let tools_config: Vec<(String, String, u64, simulacra_wasm::WasiToolConfig)> = wasm_config
            .tools
            .iter()
            .map(|tc| {
                let wasi = simulacra_wasm::WasiToolConfig {
                    fs: tc
                        .wasi
                        .fs
                        .iter()
                        .map(|m| simulacra_wasm::WasiMount {
                            host: m.host.clone(),
                            guest: m.guest.clone(),
                            perms: m.perms.clone(),
                        })
                        .collect(),
                    env: tc.wasi.env.clone(),
                };
                (tc.name.clone(), tc.module.clone(), tc.fuel, wasi)
            })
            .collect();

        for tool in simulacra_wasm::create_wasm_tools(&tools_config, None) {
            registry.register(tool);
        }
    }

    // Register Python tool (feature-gated).
    #[cfg(feature = "python")]
    {
        registry.register(Box::new(simulacra_python::PyExecTool::new(Arc::clone(
            &cell,
        ))));
    }

    // S029: Populate /proc/tools with the fully-built registry.
    shared_tools.set(registry.definitions());

    // S019: Create activity event channel. In interactive mode, events flow
    // through a ChannelActivitySink to the renderer. In headless mode we use
    // NoopActivitySink and no receiver.
    let (activity_sink, activity_rx): (
        Option<Arc<dyn simulacra_runtime::ActivitySink>>,
        Option<tokio::sync::mpsc::UnboundedReceiver<simulacra_types::ActivityEvent>>,
    ) = if mode == CliMode::Interactive {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Some(Arc::new(simulacra_runtime::ChannelActivitySink::new(tx))),
            Some(rx),
        )
    } else {
        (None, None)
    };

    // Register the spawn_agent tool when the entry agent has can_spawn configured.
    // The tool is registered in the ToolRegistry so AgentLoop can execute it.
    // It communicates with the supervisor via an mpsc channel.
    let mut spawn_rx = None;
    let mut spawn_tx_for_factory = None;
    if !agent_type.can_spawn.is_empty() {
        let (spawn_tx, rx) = tokio::sync::mpsc::channel(16);
        let spawn_sink: Arc<dyn simulacra_runtime::ActivitySink> = activity_sink
            .clone()
            .unwrap_or_else(|| Arc::new(simulacra_runtime::NoopActivitySink));
        let spawn_tx_clone = spawn_tx.clone();
        registry.register(Box::new(SpawnAgentTool {
            sender: spawn_tx,
            can_spawn: agent_type.can_spawn.clone(),
            activity_sink: spawn_sink,
            parent_id: AgentId(entry_agent.clone()),
            tiers: config.tiers.clone(),
            parent_budget: Arc::clone(&budget_arc),
            parent_model: model.clone(),
        }));
        spawn_rx = Some(rx);
        spawn_tx_for_factory = Some(spawn_tx_clone);
    }

    let tool_definitions = registry.definitions();

    // S042 Inc 3 Task 12: derive catalog mode + fixtures synchronously here.
    // `state_dir` defaults to `./.simulacra` per the v1 plan; the path is only
    // consulted in default (catalog-backed) mode. In `--no-catalog` mode,
    // we materialise the in-memory fixtures up front so tests can poke at
    // them without round-tripping through `ensure_catalog`.
    let catalog_state_dir = PathBuf::from("./.simulacra");
    let catalog_mode = catalog_import::plan_catalog_mode(args, &config, &catalog_state_dir);
    let fixtures = if matches!(catalog_mode, catalog_import::CatalogMode::NoCatalog) {
        Some(catalog_import::fixtures_from_config(&config))
    } else {
        None
    };

    Ok(CliBootstrap {
        config,
        mode,
        task,
        entry_agent,
        model,
        capability_token,
        resource_budget,
        vfs,
        tool_definitions,
        provider_kind,
        tracing_plan,
        tool_registry: registry,
        journal,
        budget_arc,
        proc_turn,
        spawn_rx,
        spawn_tx: spawn_tx_for_factory,
        activity_sink,
        activity_rx,
        skill_catalog,
        project_root,
        pipeline,
        memory_runtime,
        memory_bootstrap_info,
        integration_registry_for_refresh: integration_registry,
        catalog_mode,
        fixtures,
    })
}

/// Guard that flushes OTLP trace, metric, and log providers on drop.
struct OtelGuard {
    tracer_provider: opentelemetry_sdk::trace::SdkTracerProvider,
    meter_provider: opentelemetry_sdk::metrics::SdkMeterProvider,
    logger_provider: opentelemetry_sdk::logs::SdkLoggerProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        // Force-flush the log pipeline first. The batch log processor may
        // hold buffered records that its shutdown handler could miss on
        // ARM due to a stale `current_batch_size` read (Relaxed ordering).
        if let Err(e) = self.logger_provider.force_flush() {
            eprintln!("simulacra: failed to force-flush OTLP logs: {e}");
        }

        // Shut down metrics first (no events emitted during shutdown).
        if let Err(e) = self.meter_provider.shutdown() {
            eprintln!("simulacra: failed to flush OTLP metrics: {e}");
        }
        // Shut down tracer second.
        if let Err(e) = self.tracer_provider.shutdown() {
            eprintln!("simulacra: failed to flush OTLP traces: {e}");
        }
        // Shut down logger last so log events from shutdown are captured.
        if let Err(e) = self.logger_provider.shutdown() {
            eprintln!("simulacra: failed to flush OTLP logs: {e}");
        }
    }
}

/// Initialize the global tracing subscriber based on the tracing plan.
///
/// Returns an optional guard that MUST be held until process exit.
/// When the guard is dropped, it flushes and shuts down all OTLP exporters
/// (metrics, traces, and logs).
fn init_tracing(plan: &TracingPlan) -> Result<Option<OtelGuard>> {
    use tracing_subscriber::Layer as _;

    match plan.backend {
        TracingBackend::StderrFmt => {
            let env_filter = tracing_subscriber::EnvFilter::try_new(&plan.level)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("INFO"));
            let subscriber = tracing_subscriber::registry().with(env_filter).with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_target(false),
            );
            // Ignore error if a global subscriber is already set (e.g. in tests).
            let _ = tracing::subscriber::set_global_default(subscriber);
            Ok(None)
        }
        TracingBackend::Otlp => {
            let endpoint = plan
                .otlp_endpoint
                .as_deref()
                .unwrap_or("http://localhost:4318");

            let resource = opentelemetry_sdk::Resource::builder()
                .with_service_name("simulacra")
                .build();

            // --- Traces ---
            let trace_endpoint = format!("{endpoint}/v1/traces");
            let span_exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_endpoint(&trace_endpoint)
                .build()
                .context("failed to build OTLP span exporter")?;

            let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(span_exporter)
                .with_resource(resource.clone())
                .build();

            let otel_trace_layer =
                tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("simulacra"));

            // --- Logs (OTLP) ---
            let log_endpoint = format!("{endpoint}/v1/logs");
            let log_exporter = opentelemetry_otlp::LogExporter::builder()
                .with_http()
                .with_endpoint(&log_endpoint)
                .build()
                .context("failed to build OTLP log exporter")?;

            let logger_provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
                .with_batch_exporter(log_exporter)
                .with_resource(resource.clone())
                .build();

            let otel_log_layer =
                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                    &logger_provider,
                );

            // --- Metrics ---
            let metric_endpoint = format!("{endpoint}/v1/metrics");
            let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .with_endpoint(&metric_endpoint)
                .build()
                .context("failed to build OTLP metric exporter")?;

            let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
                .with_periodic_exporter(metric_exporter)
                .with_resource(resource)
                .build();

            // Register global meter provider so any crate can create meters
            opentelemetry::global::set_meter_provider(meter_provider.clone());

            // Use per-layer filters instead of a single global EnvFilter.
            //
            // The OTLP log bridge always uses DEBUG level so it captures all
            // application events *and* the OTel SDK's own internal-logs events
            // (which emit at DEBUG via tracing). Without this, the bridge would
            // be starved of events at the default INFO level — short-lived runs
            // that fail before emitting any INFO events would produce zero OTLP
            // log records.
            //
            // Noisy SDK targets (opentelemetry, hyper, reqwest, h2) are
            // suppressed to WARN to avoid feedback loops and log spam.
            //
            // The trace layer and stderr fmt layer honour the user-requested
            // level (INFO by default, DEBUG with --verbose).
            let user_filter = tracing_subscriber::EnvFilter::try_new(&plan.level)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("INFO"));
            let log_bridge_filter = tracing_subscriber::filter::Targets::new()
                .with_default(tracing::Level::DEBUG)
                .with_target("opentelemetry", tracing::Level::WARN)
                .with_target("hyper", tracing::Level::WARN)
                .with_target("reqwest", tracing::Level::WARN)
                .with_target("h2", tracing::Level::WARN)
                .with_target("tonic", tracing::Level::WARN);

            let subscriber = tracing_subscriber::registry()
                .with(otel_trace_layer.with_filter(user_filter))
                .with(otel_log_layer.with_filter(log_bridge_filter))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_target(false)
                        .with_filter(
                            tracing_subscriber::EnvFilter::try_new(&plan.level)
                                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("INFO")),
                        ),
                );

            // Ignore error if a global subscriber is already set (e.g. in tests).
            let _ = tracing::subscriber::set_global_default(subscriber);

            Ok(Some(OtelGuard {
                tracer_provider,
                meter_provider,
                logger_provider,
            }))
        }
    }
}

pub fn run(args: CliArgs) -> Result<CliOutput, CliError> {
    // Initialize tracing BEFORE bootstrap so MCP discovery, hook
    // pipeline build-out, and config validation events are visible to
    // operators. Previously tracing was initialized inside `run_booted`
    // (after `bootstrap` returned), which silently dropped every
    // bootstrap-time log including "MCP tool discovery complete".
    let early_plan = TracingPlan {
        backend: if args.otlp_endpoint.is_some() {
            TracingBackend::Otlp
        } else {
            TracingBackend::StderrFmt
        },
        level: if args.verbose { "DEBUG" } else { "INFO" }.to_string(),
        otlp_endpoint: args.otlp_endpoint.clone(),
    };
    let early_guard = init_tracing(&early_plan).ok().flatten();

    let boot = match bootstrap(&args) {
        Err(e) => {
            return Ok(CliOutput {
                stdout_content: String::new(),
                stderr_content: e.to_string(),
                exit_code: 1,
                telemetry_flushed: false,
            });
        }
        Ok(b) => b,
    };

    let provider = match build_provider(&boot) {
        Ok(p) => p,
        Err(e) => {
            return Ok(CliOutput {
                stdout_content: String::new(),
                stderr_content: e.to_string(),
                exit_code: 1,
                telemetry_flushed: false,
            });
        }
    };

    run_booted(args, boot, provider, early_guard)
}

pub fn run_with_provider(
    args: CliArgs,
    provider: Box<dyn Provider>,
) -> Result<CliOutput, CliError> {
    let boot = match bootstrap(&args) {
        Err(e) => {
            return Ok(CliOutput {
                stdout_content: String::new(),
                stderr_content: e.to_string(),
                exit_code: 1,
                telemetry_flushed: false,
            });
        }
        Ok(b) => b,
    };

    run_booted(args, boot, provider, None)
}

fn run_booted(
    args: CliArgs,
    mut boot: CliBootstrap,
    provider: Box<dyn Provider>,
    early_guard: Option<OtelGuard>,
) -> Result<CliOutput, CliError> {
    let has_otlp = args.otlp_endpoint.is_some();
    let verbose = args.verbose;
    let config_path = args.config_path.clone();

    // If `run` already initialized tracing, reuse that guard. Only fall
    // back to building a guard here when no early init happened (e.g.
    // `run_with_provider` from a test harness that hasn't called
    // `init_tracing`).
    let _otel_guard = match early_guard {
        Some(g) => Some(g),
        None => init_tracing(&boot.tracing_plan)?,
    };

    let task_for_span = if boot.task.len() > 100 {
        &boot.task[..100]
    } else {
        &boot.task
    };

    let project_root_str = boot.project_root.to_string_lossy().to_string();
    let cli_span = tracing::info_span!(
        "cli_run",
        "simulacra.operation.name" = "cli_run",
        "simulacra.task" = task_for_span,
        "simulacra.config.path" = config_path.as_str(),
        "simulacra.project.root" = project_root_str.as_str(),
    );

    let _cli_guard = cli_span.enter();

    // S038: Emit `memory_bootstrap` span as a child of `cli_run` once we are
    // inside the cli_run guard. The bootstrap-time payload was captured in
    // sync preflight; the span is emitted here so its parent is correct.
    if let Some(info) = boot.memory_bootstrap_info.as_ref() {
        let _mem_span = tracing::info_span!(
            "memory_bootstrap",
            "simulacra.memory.dir" = %info.dir,
            "simulacra.memory.tenant" = %info.tenant,
            "simulacra.memory.embedder_id" = %info.embedder_id,
            "simulacra.memory.embedder_dim" = info.embedder_dim,
            "simulacra.memory.entry_agent_enabled" = info.entry_agent_enabled,
            "simulacra.memory.outcome" = info.outcome,
        )
        .entered();
        tracing::info!(
            "simulacra.memory.dir" = %info.dir,
            "simulacra.memory.tenant" = %info.tenant,
            "simulacra.memory.embedder_id" = %info.embedder_id,
            "simulacra.memory.embedder_dim" = info.embedder_dim,
            "simulacra.memory.entry_agent_enabled" = info.entry_agent_enabled,
            "simulacra.memory.outcome" = info.outcome,
            "memory_bootstrap"
        );
    }

    // Capture values needed for interactive session config before moving budget
    let max_tokens = boot.resource_budget.max_tokens;
    let max_turns = boot.resource_budget.max_turns;

    // Build agent loop config
    let agent_loop_config = AgentLoopConfig {
        agent_id: AgentId(boot.entry_agent.clone()),
        system_prompt: boot
            .config
            .agent_types
            .get(&boot.entry_agent)
            .and_then(|a| a.system_prompt.clone())
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string()),
        model: boot.model.clone(),
        max_turns,
        capability: boot.capability_token.clone(),
    };

    // Clone journal and budget before they're moved into the agent loop,
    // so the supervisor can use them for child agents.
    let journal_for_supervisor = Arc::clone(&boot.journal);
    let budget_for_supervisor = boot.resource_budget.clone();
    let vfs_for_supervisor = Arc::clone(&boot.vfs);

    let activity_sink = boot.activity_sink.clone();
    let activity_rx = boot.activity_rx.take();

    let mut agent_loop = AgentLoop::new(
        agent_loop_config,
        provider,
        boot.tool_registry,
        Box::new(simulacra_context::ObservationMaskingStrategy::new(5)),
        boot.journal,
        boot.resource_budget,
        activity_sink.clone(),
        Some(Arc::clone(&boot.pipeline)),
    );
    agent_loop.set_proc_budget_mirror(Arc::clone(&boot.budget_arc), Arc::clone(&boot.proc_turn));

    // Build and run tokio multi-thread runtime
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    // S038: Spawn the BackgroundEmbedder if memory is wired. Must be done
    // inside the tokio runtime since `BackgroundEmbedder::spawn` calls
    // `tokio::spawn`.
    let memory_runtime = boot.memory_runtime.take();
    let mut retention_reaper: Option<simulacra_memory::RetentionReaper> = None;
    let embedder_handle: Option<BackgroundEmbedder> = if let Some(state) = memory_runtime {
        let MemoryRuntimeState {
            tenant,
            store,
            index,
            embedder,
            chunker_selector,
            retention,
        } = state;
        let spawn_result = runtime.block_on(async {
            BackgroundEmbedder::spawn(
                Arc::clone(&store),
                Arc::clone(&index),
                Arc::clone(&embedder),
                chunker_selector,
                BackgroundEmbedderConfig::default(),
            )
        });
        let handle = match spawn_result {
            Ok(handle) => handle,
            Err(e) => {
                let mut stderr_content = String::new();
                stderr_content.push_str(&format!("memory: background embedder spawn failed: {e}"));
                return Ok(CliOutput {
                    stdout_content: String::new(),
                    stderr_content,
                    exit_code: 1,
                    telemetry_flushed: has_otlp,
                });
            }
        };

        // S037 §20: spawn the RetentionReaper if `[memory.retention]` was
        // configured. One reaper per CLI process, one registered tenant
        // (the CLI's configured tenant). Shutdown is coordinated below
        // alongside the embedder shutdown.
        if let Some(reaper_cfg) = retention {
            let reaper = simulacra_memory::RetentionReaper::new(reaper_cfg, store, index);
            reaper.register_tenant(tenant);
            retention_reaper = Some(reaper);
        }

        Some(handle)
    } else {
        None
    };
    let mut embedder_handle = embedder_handle;

    // S033: Start background OAuth2 token refresh now that we are inside the
    // real tokio runtime. `start_background_refresh` calls `tokio::spawn`, so
    // it must run here rather than in the sync `bootstrap()` function.
    if let Some(ref reg) = boot.integration_registry_for_refresh {
        runtime.block_on(reg.start_background_refresh());
    }

    let mut stderr_content = String::new();
    if verbose {
        // In verbose mode, we indicate DEBUG-level output was produced
        stderr_content.push_str("DEBUG simulacra_cli: verbose mode enabled\n");
    }

    match boot.mode {
        CliMode::Interactive => {
            let terminal_io = match TerminalIo::new() {
                Ok(io) => io,
                Err(e) => {
                    stderr_content.push_str(&format!("failed to initialize terminal: {e}"));
                    return Ok(CliOutput {
                        stdout_content: String::new(),
                        stderr_content,
                        exit_code: 1,
                        telemetry_flushed: has_otlp,
                    });
                }
            };

            let can_spawn = boot
                .config
                .agent_types
                .get(&boot.entry_agent)
                .map(|a| a.can_spawn.clone())
                .unwrap_or_default();
            let session_config = InteractiveSessionConfig {
                project_name: boot.config.project.name.clone(),
                model: boot.model.clone(),
                max_tokens,
                max_turns,
                task: if boot.task.is_empty() {
                    None
                } else {
                    Some(boot.task.clone())
                },
                requested_session_id: args.session.clone(),
                tool_definitions: boot.tool_definitions.clone(),
                can_spawn,
                skill_catalog: boot.skill_catalog.clone(),
            };

            let storage: Arc<dyn simulacra_runtime::SessionStorage> =
                if let Some(home) = std::env::var_os("HOME") {
                    let base = std::path::PathBuf::from(home).join(".simulacra/sessions");
                    Arc::new(simulacra_runtime::FileSessionStorage::new(base))
                } else {
                    Arc::new(simulacra_runtime::InMemorySessionStorage::new())
                };

            // ProviderWrapper is a placeholder — interactive mode uses AgentLoop directly
            let provider_wrapper = Arc::new(ProviderWrapper(Mutex::new(None)));
            let mut session = InteractiveSession::new(
                terminal_io,
                provider_wrapper,
                storage,
                Arc::clone(&boot.vfs),
                session_config,
            );

            // If the user supplied --session with an id that already has a
            // persisted checkpoint, restore messages and VFS state from it.
            // Previously the session would start empty even for a resumable
            // id, effectively throwing away prior conversation history.
            if let Some(ref sid) = args.session {
                session.resume_from_storage(sid);
            }

            // Start the supervisor actor loop if spawn_agent is configured.
            // The supervisor runs as a background tokio task, receiving spawn
            // messages from the SpawnAgentTool via the mpsc channel.
            if let Some(spawn_rx) = boot.spawn_rx.take() {
                let supervisor_sink: Arc<dyn simulacra_runtime::ActivitySink> = activity_sink
                    .clone()
                    .unwrap_or_else(|| Arc::new(simulacra_runtime::NoopActivitySink));
                let child_cell_configurator =
                    boot.integration_registry_for_refresh.as_ref().map(|reg| {
                        let reg = Arc::clone(reg);
                        let tenant_integrations = boot
                            .config
                            .tenants
                            .values()
                            .find(|t| t.agent_type == boot.entry_agent)
                            .and_then(|t| t.integrations.clone())
                            .unwrap_or_default();
                        Arc::new(move |cell: &mut simulacra_sandbox::AgentCell| {
                            cell.integration_registry = Some(Arc::clone(&reg));
                            cell.tenant_integrations = tenant_integrations.clone();
                        }) as simulacra_runtime::ChildCellConfigurator
                    });
                let child_tool_registrar: Option<simulacra_runtime::ChildToolRegistrar> = {
                    #[cfg(feature = "python")]
                    {
                        Some(Arc::new(
                            |registry: &mut simulacra_tool::ToolRegistry,
                             cell: Arc<simulacra_sandbox::AgentCell>| {
                                registry
                                    .register(Box::new(simulacra_python::PyExecTool::new(cell)));
                            },
                        ))
                    }
                    #[cfg(not(feature = "python"))]
                    {
                        None
                    }
                };
                let task_factory = Arc::new(AgentTaskFactory {
                    config: boot.config.clone(),
                    provider_kind: boot.provider_kind.clone(),
                    vfs: vfs_for_supervisor,
                    journal: journal_for_supervisor,
                    activity_sink: supervisor_sink,
                    parent_capability: boot.capability_token.clone(),
                    supervisor_sender: boot.spawn_tx.clone(),
                    parent_model: boot.model.clone(),
                    pipeline: Some(Arc::clone(&boot.pipeline)),
                    script_executor: Some(simulacra_sandbox::ScriptExecutor::new(4)),
                    child_cell_configurator,
                    child_tool_registrar,
                });
                let mut supervisor = simulacra_runtime::AgentSupervisor::with_task_factory(
                    boot.capability_token.clone(),
                    budget_for_supervisor,
                    task_factory,
                );
                supervisor.set_activity_sink(activity_sink.clone().unwrap_or_else(|| {
                    Arc::new(simulacra_runtime::NoopActivitySink)
                        as Arc<dyn simulacra_runtime::ActivitySink>
                }));
                runtime.spawn(async move {
                    supervisor.run_actor_loop(spawn_rx).await;
                });
            }

            let integration_for_shutdown = boot.integration_registry_for_refresh.clone();
            let (stdout_content, exit_code, shutdown_result) = runtime.block_on(async move {
                let (stdout_content, exit_code) = session
                    .run_interactive_loop(&mut agent_loop, activity_rx)
                    .await;
                // S033: Stop background OAuth2 refresh tasks before returning.
                if let Some(ref reg) = integration_for_shutdown {
                    reg.shutdown().await;
                }
                // S037 §20: shut down the retention reaper before the
                // embedder so in-flight deletions can drain cleanly.
                if let Some(reaper) = retention_reaper.take()
                    && let Err(e) = reaper.shutdown().await
                {
                    tracing::warn!(error = %e, "retention reaper shutdown error");
                }
                let shutdown_result = if let Some(handle) = embedder_handle.take() {
                    handle.shutdown().await
                } else {
                    Ok(())
                };
                (stdout_content, exit_code, shutdown_result)
            });

            if let Err(ref e) = shutdown_result {
                tracing::warn!(error = %e, "background embedder shutdown reported an error");
                // S038 review B3: surface shutdown errors alongside the
                // agent result instead of silently swallowing them. The
                // spec lifecycle AC demands the error be "reported
                // alongside" — logging is not enough for tests and
                // operators that consume CliOutput.
                if !stderr_content.is_empty() && !stderr_content.ends_with('\n') {
                    stderr_content.push('\n');
                }
                stderr_content.push_str(&format!("memory: background embedder shutdown: {e}\n"));
            }

            Ok(CliOutput {
                stdout_content,
                stderr_content,
                exit_code,
                telemetry_flushed: has_otlp,
            })
        }
        CliMode::Headless => {
            // S038: Run the agent loop and unconditionally shut down the
            // BackgroundEmbedder afterwards (success or failure). The shutdown
            // MUST happen on both paths so background work is drained before
            // run_booted returns.
            let integration_for_shutdown = boot.integration_registry_for_refresh.clone();
            let (agent_result, shutdown_result) = runtime.block_on(async move {
                let agent_result = agent_loop.run(&boot.task).await;
                // S033: Stop background OAuth2 refresh tasks before returning.
                if let Some(ref reg) = integration_for_shutdown {
                    reg.shutdown().await;
                }
                // S037 §20: shut down the retention reaper before the
                // embedder so in-flight deletions can drain cleanly.
                if let Some(reaper) = retention_reaper.take()
                    && let Err(e) = reaper.shutdown().await
                {
                    tracing::warn!(error = %e, "retention reaper shutdown error");
                }
                let shutdown_result = if let Some(handle) = embedder_handle.take() {
                    handle.shutdown().await
                } else {
                    Ok(())
                };
                (agent_result, shutdown_result)
            });

            if let Err(ref e) = shutdown_result {
                tracing::warn!(error = %e, "background embedder shutdown reported an error");
                // S038 review B3: surface the shutdown error in stderr so
                // it's visible alongside (not instead of) the agent
                // result. Agent result still wins on exit_code — the
                // embedder drain is a secondary concern.
                if !stderr_content.is_empty() && !stderr_content.ends_with('\n') {
                    stderr_content.push('\n');
                }
                stderr_content.push_str(&format!("memory: background embedder shutdown: {e}\n"));
            }

            match agent_result {
                Ok(output) => Ok(CliOutput {
                    stdout_content: output
                        .messages
                        .last()
                        .map(|m| m.content.clone())
                        .unwrap_or_default(),
                    stderr_content,
                    exit_code: 0,
                    telemetry_flushed: has_otlp,
                }),
                Err(e) => {
                    stderr_content.push_str(&e.to_string());
                    Ok(CliOutput {
                        stdout_content: String::new(),
                        stderr_content,
                        exit_code: 1,
                        telemetry_flushed: has_otlp,
                    })
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_provider(boot: &CliBootstrap) -> Result<Box<dyn Provider>> {
    match boot.provider_kind {
        ProviderKind::Anthropic => {
            let api_key = std::env::var("ANTHROPIC_API_KEY")
                .context("ANTHROPIC_API_KEY not set. Required for claude-* models.")?;
            Ok(Box::new(AnthropicProvider::new(api_key, &boot.model)))
        }
        ProviderKind::OpenAI => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .context("OPENAI_API_KEY not set. Required for OpenAI-compatible models (Groq, Together, etc.). Set OPENAI_BASE_URL for non-OpenAI endpoints.")?;
            Ok(Box::new(OpenAiProvider::new(api_key, &boot.model)))
        }
        ProviderKind::Ollama => {
            // Ollama uses OpenAI-compatible API with no auth
            Ok(Box::new(OpenAiProvider::new("ollama", &boot.model)))
        }
    }
}

/// Validate a user-supplied session id before it is used as a filesystem
/// path component. Session ids must match `^[a-zA-Z0-9_-]+$`: this rejects
/// path-traversal attempts (`../`, `..\\`), absolute paths, dotfiles, and
/// any other component that could escape the `~/.simulacra/sessions` root.
///
/// The underlying file-backed storage also validates lexically, but
/// rejecting early produces a clear CLI-level error and keeps invalid ids
/// out of logs and telemetry.
fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() {
        bail!("invalid --session value: must not be empty");
    }
    if !session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(
            "invalid --session value {session_id:?}: only ASCII letters, digits, '_' and '-' are allowed"
        );
    }
    Ok(())
}

fn load_config(args: &CliArgs, mode: CliMode) -> Result<SimulacraConfig> {
    let path = &args.config_path;

    match SimulacraConfig::from_file(path) {
        Ok(config) => Ok(config),
        Err(simulacra_config::ConfigError::Io(e)) => {
            // Only NotFound is treated as "no config present" — other IO
            // errors (permission denied, I/O failure on a typo'd path that
            // happens to point at an unreadable file, etc.) must NOT fall
            // back to the permissive default_config, which would silently
            // enable shell, javascript, and /** read/write.
            if e.kind() == std::io::ErrorKind::NotFound {
                if args.task.is_some() {
                    Ok(default_config(args.task.as_deref().unwrap()))
                } else if mode == CliMode::Interactive {
                    Ok(default_config(args.task.as_deref().unwrap_or("")))
                } else {
                    bail!("config file not found: {path}")
                }
            } else {
                bail!("failed to read config file {path}: {e}")
            }
        }
        Err(simulacra_config::ConfigError::Parse(e)) => {
            bail!("failed to parse TOML: {e}")
        }
        Err(simulacra_config::ConfigError::Validation(e)) => {
            bail!("config validation failed: {e}")
        }
        Err(simulacra_config::ConfigError::MissingModule(name)) => {
            bail!("config validation failed: wasm MCP server {name:?} requires a module path")
        }
        Err(simulacra_config::ConfigError::WasmUrlConflict(name)) => {
            bail!(
                "config validation failed: wasm MCP server {name:?} cannot set both module and url"
            )
        }
    }
}

fn default_config(task: &str) -> SimulacraConfig {
    let model = std::env::var("SIMULACRA_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());

    let mut agent_types = HashMap::new();
    agent_types.insert(
        "default".to_string(),
        AgentTypeConfig {
            model,
            system_prompt: None,
            skills: vec![],
            max_turns: Some(50),
            max_tokens: Some(200_000),
            max_sub_agents: None,
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                shell: true,
                javascript: true,
                python: false,
                network: vec![],
                mcp: vec![],
                paths_read: vec!["/**".into()],
                paths_write: vec!["/**".into()],

                memory: None,
            }),
        },
    );

    SimulacraConfig {
        project: ProjectConfig {
            name: "simulacra-adhoc".into(),
            description: None,
        },
        agent_types,
        integrations: HashMap::new(),
        tenants: HashMap::new(),
        mcp: None,
        task: Some(TaskConfig {
            entry_agent: "default".into(),
            mode: None,
            task: Some(task.into()),
        }),
        vfs: VfsConfig::default(),
        tiers: Default::default(),
        wasm: None,
        hooks: None,
        memory: None,
        catalog: CatalogConfig::default(),
    }
}

// ---------------------------------------------------------------------------
// S029: ProcFs adapters
//
// simulacra-vfs defines ToolLister and HookLister as narrow traits so it doesn't
// take a hard dependency on simulacra-tool or simulacra-hooks. simulacra-cli depends on
// both, so it's the right place to bridge them.
// ---------------------------------------------------------------------------

/// Bridges a shared `Vec<ToolDefinition>` into the `ToolLister` trait.
///
/// Populated after the ToolRegistry is fully built so ProcFs can be wired
/// before the registry exists (chicken-and-egg avoidance).
#[derive(Clone, Default)]
struct SharedToolList(Arc<Mutex<Vec<ToolDefinition>>>);

impl SharedToolList {
    fn set(&self, defs: Vec<ToolDefinition>) {
        *self.0.lock().unwrap() = defs;
    }
}

impl ToolLister for SharedToolList {
    fn tool_names(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.name.clone())
            .collect()
    }
    fn tool_json(&self, name: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.name == name)
            .and_then(|d| serde_json::to_string(d).ok())
    }
}

/// Bridges `Arc<HookPipeline>` into the `HookLister` trait.
struct PipelineHookLister(Arc<simulacra_hooks::HookPipeline>);

impl HookLister for PipelineHookLister {
    fn hook_names(&self, operation: &str) -> Vec<String> {
        use simulacra_hooks::verdict::Operation;
        let op = match operation {
            "tool_call" => Operation::ToolCall,
            "llm" => Operation::Llm,
            "spawn" => Operation::Spawn,
            "http_request" => Operation::HttpRequest,
            "vfs_write" => Operation::VfsWrite,
            _ => return vec![],
        };
        self.0.hook_names(op)
    }
}

/// Bridges `IntegrationRegistry` into the `IntegrationLister` trait for ServiceFs.
///
/// The registry is created at startup from `[integrations.*]` config.
/// If no integrations are configured, this serves empty results.
struct RegistryIntegrationLister {
    registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
}

impl IntegrationLister for RegistryIntegrationLister {
    fn integration_names(&self) -> Vec<String> {
        self.registry
            .as_ref()
            .map(|r| r.names())
            .unwrap_or_default()
    }

    fn integration_metadata(&self, name: &str) -> Option<String> {
        self.registry
            .as_ref()?
            .metadata(name)
            .and_then(|m| serde_json::to_string(&m).ok())
    }

    fn integration_readme(&self, name: &str) -> Option<String> {
        let meta = self.registry.as_ref()?.metadata(name)?;
        let desc = meta
            .description
            .unwrap_or_else(|| format!("{name} integration"));
        Some(format!(
            "# {name}\n\n{desc}\n\n**Base URL:** {}\n**Status:** {}\n",
            meta.base_url, meta.status
        ))
    }

    fn integration_skill_names(&self, _name: &str) -> Vec<String> {
        // Skills are mounted via host VFS at /var/skills/<name>/ — not via the registry.
        // ServiceFs shows them by delegating list_dir to the inner VFS.
        vec![]
    }
}

// ---------------------------------------------------------------------------
// S020: Project root detection and host mount helpers
// ---------------------------------------------------------------------------

/// Check whether bootstrap will use ad-hoc mode (no config file found).
fn load_config_result_is_adhoc(args: &CliArgs) -> bool {
    !std::path::Path::new(&args.config_path).exists()
}
