//! Configuration types for Simulacra projects.
//!
//! Deserializes `simulacra.toml` into typed structs covering project metadata,
//! agent type definitions, MCP server lists, and task configuration.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use simulacra_types::{CapabilityToken, NetworkPermission, PathPattern};
use thiserror::Error;

pub type TierMap = IndexMap<String, String>;

/// Errors that can occur when loading a Simulacra configuration file.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse TOML: {0}")]
    Parse(#[from] toml::de::Error),

    /// Semantic validation failure — the TOML parsed fine but contains
    /// invalid values (unknown enum variants, duplicates, etc.).
    #[error("invalid config: {0}")]
    Validation(String),

    #[error("missing module for MCP server {0}")]
    MissingModule(String),

    #[error("url must be absent for wasm MCP server {0}")]
    WasmUrlConflict(String),
}

/// Top-level configuration, corresponding to the entire `simulacra.toml` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulacraConfig {
    pub project: ProjectConfig,
    pub agent_types: HashMap<String, AgentTypeConfig>,
    #[serde(default)]
    pub integrations: HashMap<String, IntegrationConfig>,
    #[serde(default)]
    pub tenants: HashMap<String, TenantConfig>,
    #[serde(default)]
    pub mcp: Option<McpConfig>,
    #[serde(default)]
    pub task: Option<TaskConfig>,
    #[serde(default)]
    pub vfs: VfsConfig,
    /// Optional `[tiers]` section mapping tier names to model identifiers.
    #[serde(default)]
    pub tiers: TierMap,
    /// Optional `[wasm]` section for WASM tool modules.
    #[serde(default)]
    pub wasm: Option<WasmConfig>,
    /// Optional `[hooks]` section for governance hook pipeline.
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
    /// S038: Optional top-level `[memory]` section. When present and the
    /// entry agent's `MemoryCapability.enabled` is true, the CLI bootstrap
    /// wires the memory subsystem. When absent, the CLI behaves as before.
    #[serde(default)]
    pub memory: Option<MemoryConfig>,
    /// S042: Optional `[catalog]` section selecting where the SQLite catalog
    /// lives. Defaults to `<state_dir>/catalog.db`. The CLI consults this in
    /// "default" (catalog-backed) bootstrap mode; ignored under
    /// `--no-catalog`.
    #[serde(default)]
    pub catalog: CatalogConfig,
}

/// S042 §"simulacra-cli bootstrap import": `[catalog]` section.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CatalogConfig {
    /// Override path to the SQLite catalog file. When `None`, callers should
    /// fall back to `<state_dir>/catalog.db` via [`CatalogConfig::resolved_db_path`].
    pub db_path: Option<PathBuf>,
}

impl CatalogConfig {
    /// Resolve the catalog DB path, falling back to `<state_dir>/catalog.db`
    /// when `db_path` is unset.
    pub fn resolved_db_path(&self, state_dir: &Path) -> PathBuf {
        self.db_path
            .clone()
            .unwrap_or_else(|| state_dir.join("catalog.db"))
    }
}

/// S038: Top-level memory subsystem configuration for the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Directory holding the SQLite store and vector index. Created if
    /// absent. Must be writable.
    pub dir: std::path::PathBuf,
    /// Tenant id for this CLI run. Default: "cli". Must satisfy the
    /// `TenantId` regex or startup fails.
    #[serde(default = "default_memory_tenant")]
    pub tenant: String,
    /// S037 retention configuration. `None` = no reaper runs.
    #[serde(default)]
    pub retention: Option<MemoryRetentionConfig>,
    /// S037 §13 policy for handling embedding-model changes across runs.
    #[serde(default)]
    pub on_model_change: OnModelChange,
}

/// S037 §13: policy for reconciling an embedding-model change between runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnModelChange {
    /// Refuse to start; operator must intervene explicitly.
    #[default]
    Refuse,
    /// Keep serving reads while a background job reindexes embeddings.
    ReindexBackground,
    /// Drop the existing index and rebuild from scratch.
    WipeAndRebuild,
}

fn default_memory_tenant() -> String {
    "cli".to_string()
}

/// S037 §20 Retention: per-subtree TTL configuration + reaper interval.
///
/// Example TOML:
/// ```toml
/// [memory.retention]
/// interval_secs = 3600        # default 1h
/// batch_size = 256             # per-sweep deletion cap (prevents long per-tenant locks)
///
/// [[memory.retention.subtrees]]
/// prefix = "/var/memory/ephemeral"
/// ttl_secs = 86400            # 1 day
///
/// [[memory.retention.subtrees]]
/// prefix = "/mnt/transient"
/// ttl_secs = 604800           # 7 days
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRetentionConfig {
    #[serde(default = "default_retention_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_retention_batch_size")]
    pub batch_size: u64,
    #[serde(default)]
    pub subtrees: Vec<RetentionSubtree>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionSubtree {
    /// Memory path prefix (must start with `/var/memory/` or `/mnt/`).
    pub prefix: String,
    /// Entries whose mtime is older than `ttl_secs` are deleted.
    pub ttl_secs: u64,
}

fn default_retention_interval_secs() -> u64 {
    3600
}

fn default_retention_batch_size() -> u64 {
    256
}

impl SimulacraConfig {
    /// Read and parse a `SimulacraConfig` from the TOML file at `path`.
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        let contents = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;
        config.validate()?;
        Ok(config)
    }

    /// Run semantic validation checks after TOML parsing.
    ///
    /// Verifies that:
    /// - every `[[mcp.servers]]` transport (if present) is a known value
    ///   (`"sse"`, `"http"`, or `"auto"`).
    /// - `[[mcp.servers]]` entries have unique `name`s.
    /// - every hook `runtime` is a known value (currently only `"js"`).
    ///
    /// Returns `ConfigError::Validation` with an actionable message on the
    /// first failure. Callers that build a `SimulacraConfig` directly in tests
    /// or by hand can call this to enforce the same invariants as
    /// `from_file`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // MCP server validation.
        if let Some(ref mcp) = self.mcp {
            let mut seen: std::collections::HashSet<&str> =
                std::collections::HashSet::with_capacity(mcp.servers.len());
            for server in &mcp.servers {
                if !seen.insert(server.name.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "duplicate MCP server name: {}",
                        server.name
                    )));
                }
                if let Some(ref transport) = server.transport
                    && !is_valid_mcp_transport(transport)
                {
                    return Err(ConfigError::Validation(format!(
                        "MCP server '{}': unknown transport '{}' — expected one of \"sse\", \"http\", \"auto\", \"wasm\"",
                        server.name, transport
                    )));
                }
                validate_mcp_server(server)?;
            }
        }

        // Hook runtime validation.
        if let Some(ref hooks) = self.hooks {
            for (op, entries) in [
                ("tool_call", &hooks.tool_call),
                ("llm", &hooks.llm),
                ("spawn", &hooks.spawn),
                ("http_request", &hooks.http_request),
                ("vfs_write", &hooks.vfs_write),
            ] {
                for entry in entries {
                    if !is_valid_hook_runtime(&entry.runtime) {
                        return Err(ConfigError::Validation(format!(
                            "hook '{}' (op {}): unknown runtime '{}' — expected \"js\"",
                            entry.name, op, entry.runtime
                        )));
                    }
                }
            }
        }

        Ok(())
    }
}

/// Known MCP transports. Kept in one place so validation and loading agree.
fn is_valid_mcp_transport(t: &str) -> bool {
    matches!(t, "" | "sse" | "http" | "auto" | "wasm")
}

/// Known hook runtimes. Extend here when wasm-backed hooks land.
fn is_valid_hook_runtime(r: &str) -> bool {
    matches!(r, "js")
}

/// Project-level metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Configuration for a single agent type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTypeConfig {
    pub model: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_sub_agents: Option<u32>,
    #[serde(default)]
    pub can_spawn: Vec<String>,
    #[serde(default)]
    pub restart_policy: Option<String>,
    #[serde(default)]
    pub capabilities: Option<CapabilitiesConfig>,
}

/// Capability grants for an agent type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilitiesConfig {
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
    #[serde(default)]
    pub shell: bool,
    #[serde(default)]
    pub javascript: bool,
    #[serde(default)]
    pub python: bool,
    #[serde(default)]
    pub paths_read: Vec<String>,
    #[serde(default)]
    pub paths_write: Vec<String>,
    /// Memory capability — opt-in per S037. Absent = disabled. When
    /// present and `enabled = true`, the agent gets `/var/memory/**` and
    /// `/mnt/**` subtree access per `search_scopes` / `write_scopes`, plus
    /// the `semantic_search` and `memory_read_chunk` tools registered in
    /// its ToolRegistry.
    #[serde(default)]
    pub memory: Option<MemoryCapabilityConfig>,
}

/// TOML shape for the memory capability section of an agent type.
///
/// Example:
/// ```toml
/// [agent_types.atlas.capabilities.memory]
/// enabled = true
/// search_scopes = ["/var/memory/self/", "/var/memory/entities/", "/mnt/"]
/// write_scopes = ["/var/memory/self/"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCapabilityConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub search_scopes: Vec<String>,
    #[serde(default)]
    pub write_scopes: Vec<String>,
}

/// Authentication method for an integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthMethod {
    #[serde(rename = "oauth2")]
    OAuth2 {
        client_id: String,
        client_secret: String,
        token_url: String,
        #[serde(default)]
        scopes: Vec<String>,
        #[serde(default)]
        refresh_token: Option<String>,
    },
    #[serde(rename = "api_key")]
    ApiKey {
        key: String,
        #[serde(default = "default_key_placement")]
        placement: String,
    },
}

fn default_key_placement() -> String {
    "header".to_string()
}

/// Configuration for a single integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationConfig {
    #[serde(flatten)]
    pub auth: AuthMethod,
    pub base_url: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub rate_limit_rps: u32,
    #[serde(default)]
    pub skills_path: Option<String>,
}

/// Tenant-specific integration grants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantConfig {
    pub agent_type: String,
    #[serde(default)]
    pub integrations: Option<Vec<String>>,
    /// MCP server names this tenant is allowed to use.
    ///
    /// `None` means no tenant-specific MCP grant in multi-tenant mode. In
    /// single-tenant configs with no `[tenants]` table, top-level `[mcp]`
    /// servers remain globally visible for backwards-compatible local use.
    #[serde(default)]
    pub mcp_servers: Option<Vec<String>>,
}

/// MCP server configuration block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// A single MCP server entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub wasi: Option<crate::WasiToolConfig>,
}

/// Task configuration -- what to run and how.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    pub entry_agent: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_max_files_per_mount() -> usize {
    10_000
}

fn default_max_bytes_per_mount() -> u64 {
    104_857_600
}

/// Optional `[vfs]` section in simulacra.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfsConfig {
    /// Whether to auto-mount the project `skills/` directory. Default: true.
    #[serde(default = "default_true")]
    pub auto_mount_skills: bool,

    /// Maximum number of files per mount. Default: 10_000.
    #[serde(default = "default_max_files_per_mount")]
    pub max_files_per_mount: usize,

    /// Maximum total bytes per mount. Default: 104_857_600 (100 MiB).
    #[serde(default = "default_max_bytes_per_mount")]
    pub max_bytes_per_mount: u64,

    /// Configured mount entries.
    #[serde(default)]
    pub mounts: Vec<MountConfig>,
}

impl Default for VfsConfig {
    fn default() -> Self {
        Self {
            auto_mount_skills: default_true(),
            max_files_per_mount: default_max_files_per_mount(),
            max_bytes_per_mount: default_max_bytes_per_mount(),
            mounts: Vec::new(),
        }
    }
}

/// A single `[[vfs.mounts]]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    /// Host filesystem path (relative to project root, or absolute, or ~-prefixed).
    pub source: String,

    /// Absolute VFS path where the source is mounted.
    pub target: String,
}

// ── Hooks configuration ─────────────────────────────────────────────────

/// Configuration for governance hooks (S026).
///
/// Each operation type has an ordered list of hook entries that form
/// the pipeline for that operation.
///
/// `#[non_exhaustive]`: new hook-op fields can be added without breaking
/// downstream construction. External callers must build via `Default`
/// (e.g. `HooksConfig { tool_call: ..., ..Default::default() }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HooksConfig {
    #[serde(default)]
    pub tool_call: Vec<HookEntry>,
    #[serde(default)]
    pub llm: Vec<HookEntry>,
    #[serde(default)]
    pub spawn: Vec<HookEntry>,
    #[serde(default)]
    pub http_request: Vec<HookEntry>,
    /// S039: hooks for `Operation::VfsWrite` — invoked on every VFS write or
    /// remove that traverses a `HookedVfsLayer`.
    #[serde(default)]
    pub vfs_write: Vec<HookEntry>,
}

/// A single hook entry referencing a runtime module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEntry {
    pub name: String,
    pub runtime: String,
    pub module: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout_ms: u64,
}

fn default_hook_timeout() -> u64 {
    100
}

// ── WASM tool configuration ─────────────────────────────────────────────

/// Top-level `[wasm]` section in `simulacra.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmConfig {
    #[serde(default)]
    pub tools: Vec<WasmToolEntry>,
}

/// A single `[[wasm.tools]]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmToolEntry {
    pub name: String,
    pub module: String,
    #[serde(default)]
    pub fuel: u64,
    #[serde(default)]
    pub wasi: WasiEntry,
}

/// WASI sandbox configuration for a WASM tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WasiEntry {
    #[serde(default)]
    pub fs: Vec<WasiMountEntry>,
    #[serde(default)]
    pub env: Vec<String>,
}

pub type WasiToolConfig = WasiEntry;

/// A single filesystem mount inside a WASI sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasiMountEntry {
    pub host: String,
    pub guest: String,
    #[serde(default = "default_perms")]
    pub perms: String,
}

fn default_perms() -> String {
    "ro".into()
}

/// Validate a single `[[mcp.servers]]` entry beyond TOML parsing.
///
/// For `transport = "wasm"`:
/// - `module` must be set (else `ConfigError::MissingModule(name)`).
/// - `url` must be unset (else `ConfigError::WasmUrlConflict(name)`).
///
/// Other transports / unset transport pass through unchanged.
pub fn validate_mcp_server(config: &McpServerConfig) -> Result<(), ConfigError> {
    if config.transport.as_deref() == Some("wasm") {
        if config.module.is_none() {
            return Err(ConfigError::MissingModule(config.name.clone()));
        }
        if config.url.is_some() {
            return Err(ConfigError::WasmUrlConflict(config.name.clone()));
        }
    }
    Ok(())
}

/// Build a [`CapabilityToken`] from an [`AgentTypeConfig`].
///
/// Maps the config-level capability grants into the domain-level token
/// that the runtime uses for capability enforcement.
///
/// `can_spawn` lives on `AgentTypeConfig` (not the capabilities block), so
/// it is copied onto the resulting token regardless of whether a
/// `[agent_types.X.capabilities]` block is present. Without this, an agent
/// that has `can_spawn = ["worker"]` but no capabilities block would have
/// its spawn grant silently dropped.
pub fn build_capability_token(agent_type: &AgentTypeConfig) -> CapabilityToken {
    let mut token = match &agent_type.capabilities {
        Some(caps) => CapabilityToken {
            network: caps
                .network
                .iter()
                .map(|s| NetworkPermission(s.clone()))
                .collect(),
            mcp_tools: caps.mcp.clone(),
            shell: caps.shell,
            javascript: caps.javascript,
            python: caps.python,
            paths_read: caps
                .paths_read
                .iter()
                .map(|s| PathPattern(s.clone()))
                .collect(),
            paths_write: caps
                .paths_write
                .iter()
                .map(|s| PathPattern(s.clone()))
                .collect(),
            spawn_types: Vec::new(),
            skill_patterns: vec![],
            memory: build_memory_capability(caps.memory.as_ref()),
        },
        None => CapabilityToken::default(),
    };
    token.spawn_types = agent_type.can_spawn.clone();
    token
}

/// Map `MemoryCapabilityConfig` → `simulacra_types::MemoryCapability`. Invalid
/// MemoryPath entries in `search_scopes` or `write_scopes` are dropped with
/// a warning — a typo should not silently grant broader access, but it also
/// should not crash startup. A typo in a search_scope just means the agent
/// can't search that (non-existent) scope at runtime.
fn build_memory_capability(
    config: Option<&MemoryCapabilityConfig>,
) -> simulacra_types::MemoryCapability {
    use simulacra_types::{MemoryCapability, MemoryPath};
    let Some(cfg) = config else {
        return MemoryCapability::default();
    };
    let parse_scopes = |raw: &[String]| -> Vec<MemoryPath> {
        raw.iter()
            .filter_map(|s| match MemoryPath::parse(s) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(scope = %s, error = %e, "invalid memory scope in config; ignoring");
                    None
                }
            })
            .collect()
    };
    MemoryCapability {
        enabled: cfg.enabled,
        search_scopes: parse_scopes(&cfg.search_scopes),
        write_scopes: parse_scopes(&cfg.write_scopes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_example_toml() {
        let toml_str = r#"
[project]
name = "my-automation"
description = "Automated code review pipeline"

[agent_types.planner]
model = "claude-sonnet-4-20250514"
system_prompt = "prompts/planner.md"
skills = []
max_turns = 50
max_tokens = 100_000
max_sub_agents = 5
can_spawn = ["coder", "reviewer"]
restart_policy = "retry_twice_then_fail"

[agent_types.planner.capabilities]
network = ["net:api.anthropic.com"]
mcp = ["mcp:github:*", "mcp:linear:*"]
shell = true
javascript = true

[[mcp.servers]]
name = "github"
transport = "sse"
url = "https://mcp.github.com/sse"

[task]
entry_agent = "planner"
mode = "headless"
task = "Review PR #42"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse example TOML");

        assert_eq!(config.project.name, "my-automation");
        assert_eq!(
            config.project.description.as_deref(),
            Some("Automated code review pipeline")
        );

        let planner = config.agent_types.get("planner").expect("planner agent");
        assert_eq!(planner.model, "claude-sonnet-4-20250514");
        assert_eq!(planner.max_turns, Some(50));
        assert_eq!(planner.max_tokens, Some(100_000));
        assert_eq!(planner.max_sub_agents, Some(5));
        assert_eq!(planner.can_spawn, vec!["coder", "reviewer"]);
        assert_eq!(
            planner.restart_policy.as_deref(),
            Some("retry_twice_then_fail")
        );

        let caps = planner.capabilities.as_ref().expect("capabilities");
        assert_eq!(caps.network, vec!["net:api.anthropic.com"]);
        assert!(caps.shell);
        assert!(caps.javascript);
        assert!(!caps.python);

        let mcp = config.mcp.as_ref().expect("mcp section");
        assert_eq!(mcp.servers.len(), 1);
        assert_eq!(mcp.servers[0].name, "github");
        assert_eq!(mcp.servers[0].transport.as_deref(), Some("sse"));

        let task = config.task.as_ref().expect("task section");
        assert_eq!(task.entry_agent, "planner");
        assert_eq!(task.mode.as_deref(), Some("headless"));
        assert_eq!(task.task.as_deref(), Some("Review PR #42"));
    }

    #[test]
    fn mcp_server_config_transport_is_optional() {
        let toml = r#"
[[servers]]
name = "my-server"
url = "http://localhost:3000"
"#;
        let mcp: McpConfig = toml::from_str(toml).expect("parse failed");
        assert_eq!(mcp.servers.len(), 1);
        assert_eq!(mcp.servers[0].name, "my-server");
        assert!(
            mcp.servers[0].transport.is_none(),
            "transport should be None when omitted"
        );
    }

    #[test]
    fn mcp_server_config_transport_explicit_sse_still_works() {
        let toml = r#"
[[servers]]
name = "my-server"
transport = "sse"
url = "http://localhost:3000"
"#;
        let mcp: McpConfig = toml::from_str(toml).expect("parse failed");
        assert_eq!(mcp.servers.len(), 1);
        assert_eq!(
            mcp.servers[0].transport.as_deref(),
            Some("sse"),
            "transport should be Some(\"sse\") when explicitly set"
        );
    }

    // ── C1: build_capability_token ──────────────────────────────────────

    #[test]
    fn build_capability_token_without_capabilities_returns_default() {
        let agent = AgentTypeConfig {
            model: "test-model".into(),
            system_prompt: None,
            skills: vec![],
            max_turns: None,
            max_tokens: None,
            max_sub_agents: None,
            can_spawn: vec![],
            restart_policy: None,
            capabilities: None,
        };

        let token = build_capability_token(&agent);

        assert!(token.network.is_empty());
        assert!(token.mcp_tools.is_empty());
        assert!(!token.shell);
        assert!(!token.javascript);
        assert!(!token.python);
        assert!(token.paths_read.is_empty());
        assert!(token.paths_write.is_empty());
        assert!(token.spawn_types.is_empty());
        assert!(token.skill_patterns.is_empty());
    }

    #[test]
    fn build_capability_token_maps_network_permissions() {
        let agent = AgentTypeConfig {
            model: "m".into(),
            system_prompt: None,
            skills: vec![],
            max_turns: None,
            max_tokens: None,
            max_sub_agents: None,
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec!["net:api.github.com".into(), "net:*.stripe.com".into()],
                mcp: vec![],
                shell: false,
                javascript: false,
                python: false,
                paths_read: vec![],
                paths_write: vec![],

                memory: None,
            }),
        };

        let token = build_capability_token(&agent);

        assert_eq!(token.network.len(), 2);
        assert_eq!(
            token.network[0],
            NetworkPermission("net:api.github.com".into())
        );
        assert_eq!(
            token.network[1],
            NetworkPermission("net:*.stripe.com".into())
        );
    }

    #[test]
    fn build_capability_token_maps_mcp_tools() {
        let agent = AgentTypeConfig {
            model: "m".into(),
            system_prompt: None,
            skills: vec![],
            max_turns: None,
            max_tokens: None,
            max_sub_agents: None,
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec!["mcp:github:*".into(), "mcp:linear:read".into()],
                shell: false,
                javascript: false,
                python: false,
                paths_read: vec![],
                paths_write: vec![],

                memory: None,
            }),
        };

        let token = build_capability_token(&agent);

        assert_eq!(token.mcp_tools, vec!["mcp:github:*", "mcp:linear:read"]);
    }

    #[test]
    fn build_capability_token_maps_boolean_capabilities() {
        let agent = AgentTypeConfig {
            model: "m".into(),
            system_prompt: None,
            skills: vec![],
            max_turns: None,
            max_tokens: None,
            max_sub_agents: None,
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec![],
                shell: true,
                javascript: true,
                python: true,
                paths_read: vec![],
                paths_write: vec![],

                memory: None,
            }),
        };

        let token = build_capability_token(&agent);

        assert!(token.shell);
        assert!(token.javascript);
        assert!(token.python);
    }

    #[test]
    fn build_capability_token_maps_path_patterns() {
        let agent = AgentTypeConfig {
            model: "m".into(),
            system_prompt: None,
            skills: vec![],
            max_turns: None,
            max_tokens: None,
            max_sub_agents: None,
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec![],
                shell: false,
                javascript: false,
                python: false,
                paths_read: vec!["/src/**".into(), "/docs/**".into()],
                paths_write: vec!["/src/**".into()],

                memory: None,
            }),
        };

        let token = build_capability_token(&agent);

        assert_eq!(token.paths_read.len(), 2);
        assert_eq!(token.paths_read[0], PathPattern("/src/**".into()));
        assert_eq!(token.paths_read[1], PathPattern("/docs/**".into()));
        assert_eq!(token.paths_write.len(), 1);
        assert_eq!(token.paths_write[0], PathPattern("/src/**".into()));
    }

    #[test]
    fn build_capability_token_preserves_can_spawn_without_capabilities_block() {
        // Regression guard: `can_spawn` lives on AgentTypeConfig, not inside
        // the [capabilities] block. An agent that configures `can_spawn =
        // ["worker"]` without a capabilities block should still end up with
        // `token.spawn_types == ["worker"]`. Without this behaviour the
        // planner/supervisor would refuse the spawn at runtime even though
        // the config clearly authorises it.
        let agent = AgentTypeConfig {
            model: "m".into(),
            system_prompt: None,
            skills: vec![],
            max_turns: None,
            max_tokens: None,
            max_sub_agents: None,
            can_spawn: vec!["worker".into(), "reviewer".into()],
            restart_policy: None,
            capabilities: None,
        };

        let token = build_capability_token(&agent);

        assert_eq!(token.spawn_types, vec!["worker", "reviewer"]);
    }

    #[test]
    fn build_capability_token_maps_spawn_types_from_can_spawn() {
        let agent = AgentTypeConfig {
            model: "m".into(),
            system_prompt: None,
            skills: vec![],
            max_turns: None,
            max_tokens: None,
            max_sub_agents: None,
            can_spawn: vec!["coder".into(), "reviewer".into()],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec![],
                shell: false,
                javascript: false,
                python: false,
                paths_read: vec![],
                paths_write: vec![],

                memory: None,
            }),
        };

        let token = build_capability_token(&agent);

        assert_eq!(token.spawn_types, vec!["coder", "reviewer"]);
    }

    #[test]
    fn build_capability_token_skill_patterns_always_empty() {
        // The current implementation always sets skill_patterns to vec![]
        // regardless of the agent config skills field.
        let agent = AgentTypeConfig {
            model: "m".into(),
            system_prompt: None,
            skills: vec!["skill:rust-dev".into()],
            max_turns: None,
            max_tokens: None,
            max_sub_agents: None,
            can_spawn: vec![],
            restart_policy: None,
            capabilities: Some(CapabilitiesConfig {
                network: vec![],
                mcp: vec![],
                shell: false,
                javascript: false,
                python: false,
                paths_read: vec![],
                paths_write: vec![],

                memory: None,
            }),
        };

        let token = build_capability_token(&agent);

        assert!(token.skill_patterns.is_empty());
    }

    #[test]
    fn build_capability_token_with_all_fields_populated() {
        let agent = AgentTypeConfig {
            model: "claude-sonnet-4-20250514".into(),
            system_prompt: Some("prompt.md".into()),
            skills: vec!["skill:code-review".into()],
            max_turns: Some(50),
            max_tokens: Some(100_000),
            max_sub_agents: Some(5),
            can_spawn: vec!["worker".into()],
            restart_policy: Some("retry".into()),
            capabilities: Some(CapabilitiesConfig {
                network: vec!["net:api.anthropic.com".into()],
                mcp: vec!["mcp:github:*".into()],
                shell: true,
                javascript: true,
                python: false,
                paths_read: vec!["/workspace/**".into()],
                paths_write: vec!["/workspace/src/**".into()],

                memory: None,
            }),
        };

        let token = build_capability_token(&agent);

        assert_eq!(token.network.len(), 1);
        assert_eq!(
            token.network[0],
            NetworkPermission("net:api.anthropic.com".into())
        );
        assert_eq!(token.mcp_tools, vec!["mcp:github:*"]);
        assert!(token.shell);
        assert!(token.javascript);
        assert!(!token.python);
        assert_eq!(token.paths_read, vec![PathPattern("/workspace/**".into())]);
        assert_eq!(
            token.paths_write,
            vec![PathPattern("/workspace/src/**".into())]
        );
        assert_eq!(token.spawn_types, vec!["worker"]);
        assert!(token.skill_patterns.is_empty());
    }

    // ── C2: VFS defaults ────────────────────────────────────────────────

    #[test]
    fn vfs_defaults_when_section_omitted() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert!(config.vfs.auto_mount_skills);
        assert_eq!(config.vfs.max_files_per_mount, 10_000);
        assert_eq!(config.vfs.max_bytes_per_mount, 104_857_600);
        assert!(config.vfs.mounts.is_empty());
    }

    #[test]
    fn vfs_defaults_when_section_present_but_empty() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[vfs]
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert!(config.vfs.auto_mount_skills);
        assert_eq!(config.vfs.max_files_per_mount, 10_000);
        assert_eq!(config.vfs.max_bytes_per_mount, 104_857_600);
        assert!(config.vfs.mounts.is_empty());
    }

    #[test]
    fn vfs_auto_mount_skills_can_be_disabled() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[vfs]
auto_mount_skills = false
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert!(!config.vfs.auto_mount_skills);
    }

    #[test]
    fn vfs_custom_limits_override_defaults() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[vfs]
max_files_per_mount = 500
max_bytes_per_mount = 1048576
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert_eq!(config.vfs.max_files_per_mount, 500);
        assert_eq!(config.vfs.max_bytes_per_mount, 1_048_576);
    }

    #[test]
    fn vfs_default_impl_matches_serde_defaults() {
        let vfs_default = VfsConfig::default();

        assert!(vfs_default.auto_mount_skills);
        assert_eq!(vfs_default.max_files_per_mount, 10_000);
        assert_eq!(vfs_default.max_bytes_per_mount, 104_857_600);
        assert!(vfs_default.mounts.is_empty());
    }

    // ── C3: [[vfs.mounts]] config shape ─────────────────────────────────

    #[test]
    fn vfs_mounts_parses_single_mount() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[vfs.mounts]]
source = "./skills"
target = "/skills"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert_eq!(config.vfs.mounts.len(), 1);
        assert_eq!(config.vfs.mounts[0].source, "./skills");
        assert_eq!(config.vfs.mounts[0].target, "/skills");
    }

    #[test]
    fn vfs_mounts_parses_multiple_mounts() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[vfs.mounts]]
source = "./skills"
target = "/skills"

[[vfs.mounts]]
source = "/home/user/data"
target = "/data"

[[vfs.mounts]]
source = "~/configs"
target = "/configs"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert_eq!(config.vfs.mounts.len(), 3);

        assert_eq!(config.vfs.mounts[0].source, "./skills");
        assert_eq!(config.vfs.mounts[0].target, "/skills");

        assert_eq!(config.vfs.mounts[1].source, "/home/user/data");
        assert_eq!(config.vfs.mounts[1].target, "/data");

        assert_eq!(config.vfs.mounts[2].source, "~/configs");
        assert_eq!(config.vfs.mounts[2].target, "/configs");
    }

    #[test]
    fn vfs_mounts_with_custom_limits() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[vfs]
auto_mount_skills = false
max_files_per_mount = 100
max_bytes_per_mount = 5242880

[[vfs.mounts]]
source = "./src"
target = "/workspace/src"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert!(!config.vfs.auto_mount_skills);
        assert_eq!(config.vfs.max_files_per_mount, 100);
        assert_eq!(config.vfs.max_bytes_per_mount, 5_242_880);
        assert_eq!(config.vfs.mounts.len(), 1);
        assert_eq!(config.vfs.mounts[0].source, "./src");
        assert_eq!(config.vfs.mounts[0].target, "/workspace/src");
    }

    #[test]
    fn vfs_mount_missing_source_field_is_parse_error() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[vfs.mounts]]
target = "/skills"
"#;

        let result: Result<SimulacraConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err(), "mount without source should fail to parse");
    }

    #[test]
    fn vfs_mount_missing_target_field_is_parse_error() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[vfs.mounts]]
source = "./skills"
"#;

        let result: Result<SimulacraConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err(), "mount without target should fail to parse");
    }

    // ── C4: from_file and ConfigError ───────────────────────────────────

    #[test]
    fn from_file_nonexistent_path_returns_io_error() {
        let result = SimulacraConfig::from_file("/nonexistent/path/simulacra.toml");
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("failed to read config file"),
            "expected IO error message, got: {err_msg}"
        );

        // Verify it's the Io variant
        assert!(matches!(err, ConfigError::Io(_)));
    }

    #[test]
    fn from_file_malformed_toml_returns_parse_error() {
        use std::io::Write;

        let dir = std::env::temp_dir();
        let path = dir.join("simulacra_config_test_malformed.toml");
        {
            let mut f = std::fs::File::create(&path).expect("create temp file");
            f.write_all(b"this is not [valid toml {{{{")
                .expect("write temp file");
        }

        let result = SimulacraConfig::from_file(path.to_str().unwrap());
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("failed to parse TOML"),
            "expected parse error message, got: {err_msg}"
        );

        assert!(matches!(err, ConfigError::Parse(_)));

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn from_file_valid_toml_parses_successfully() {
        use std::io::Write;

        let dir = std::env::temp_dir();
        let path = dir.join("simulacra_config_test_valid.toml");
        {
            let mut f = std::fs::File::create(&path).expect("create temp file");
            f.write_all(
                br#"
[project]
name = "test-project"

[agent_types.worker]
model = "test-model"
"#,
            )
            .expect("write temp file");
        }

        let config = SimulacraConfig::from_file(path.to_str().unwrap()).expect("should parse");
        assert_eq!(config.project.name, "test-project");

        let worker = config.agent_types.get("worker").expect("worker agent");
        assert_eq!(worker.model, "test-model");

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn from_file_empty_toml_is_parse_error() {
        use std::io::Write;

        let dir = std::env::temp_dir();
        let path = dir.join("simulacra_config_test_empty.toml");
        {
            let mut f = std::fs::File::create(&path).expect("create temp file");
            f.write_all(b"").expect("write temp file");
        }

        let result = SimulacraConfig::from_file(path.to_str().unwrap());
        assert!(
            result.is_err(),
            "empty TOML should fail (missing required fields)"
        );
        assert!(matches!(result.unwrap_err(), ConfigError::Parse(_)));

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    // ── Edge cases: minimal and full configs ────────────────────────────

    #[test]
    fn minimal_config_only_requires_project_and_agent_types() {
        let toml_str = r#"
[project]
name = "minimal"

[agent_types.a]
model = "m"
"#;

        let config: SimulacraConfig =
            toml::from_str(toml_str).expect("should parse minimal config");

        assert_eq!(config.project.name, "minimal");
        assert!(config.project.description.is_none());
        assert!(config.mcp.is_none());
        assert!(config.task.is_none());

        let agent = config.agent_types.get("a").expect("agent a");
        assert_eq!(agent.model, "m");
        assert!(agent.system_prompt.is_none());
        assert!(agent.skills.is_empty());
        assert!(agent.max_turns.is_none());
        assert!(agent.max_tokens.is_none());
        assert!(agent.max_sub_agents.is_none());
        assert!(agent.can_spawn.is_empty());
        assert!(agent.restart_policy.is_none());
        assert!(agent.capabilities.is_none());
    }

    #[test]
    fn config_missing_project_section_is_parse_error() {
        let toml_str = r#"
[agent_types.a]
model = "m"
"#;

        let result: Result<SimulacraConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err(), "config without [project] should fail");
    }

    #[test]
    fn config_missing_agent_types_section_is_parse_error() {
        let toml_str = r#"
[project]
name = "test"
"#;

        let result: Result<SimulacraConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err(), "config without [agent_types] should fail");
    }

    #[test]
    fn capabilities_defaults_all_false_and_empty() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[agent_types.worker.capabilities]
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");
        let caps = config
            .agent_types
            .get("worker")
            .unwrap()
            .capabilities
            .as_ref()
            .expect("capabilities");

        assert!(caps.network.is_empty());
        assert!(caps.mcp.is_empty());
        assert!(!caps.shell);
        assert!(!caps.javascript);
        assert!(!caps.python);
        assert!(caps.paths_read.is_empty());
        assert!(caps.paths_write.is_empty());
    }

    // ── Tiers config ───────────────────────────────────────────────────

    #[test]
    fn tiers_section_parsed() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[tiers]
reasoning = "claude-opus-4-6"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert_eq!(config.tiers.len(), 1);
        assert_eq!(config.tiers.get("reasoning").unwrap(), "claude-opus-4-6");
    }

    #[test]
    fn tiers_section_absent_defaults_to_empty() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert!(config.tiers.is_empty());
    }

    #[test]
    fn tiers_with_custom_names() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[tiers]
reasoning = "claude-opus-4-6"
balanced = "claude-sonnet-4-20250514"
fast = "claude-haiku-35-20241022"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert_eq!(config.tiers.len(), 3);
        assert_eq!(config.tiers.get("reasoning").unwrap(), "claude-opus-4-6");
        assert_eq!(
            config.tiers.get("balanced").unwrap(),
            "claude-sonnet-4-20250514"
        );
        assert_eq!(
            config.tiers.get("fast").unwrap(),
            "claude-haiku-35-20241022"
        );
    }

    // ── WASM config ────────────────────────────────────────────────────

    #[test]
    fn wasm_tools_config_parses() {
        let toml_str = r#"
[project]
name = "wasm-test"

[agent_types.worker]
model = "m"

[[wasm.tools]]
name = "echo"
module = "./tools/echo-tool.wasm"
fuel = 500000

[[wasm.tools.wasi.fs]]
host = "/tmp/data"
guest = "/data"
perms = "rw"

[wasm.tools.wasi]
env = ["FOO=bar"]
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse wasm config");

        let wasm = config.wasm.expect("wasm section should be present");
        assert_eq!(wasm.tools.len(), 1);

        let tool = &wasm.tools[0];
        assert_eq!(tool.name, "echo");
        assert_eq!(tool.module, "./tools/echo-tool.wasm");
        assert_eq!(tool.fuel, 500_000);
        assert_eq!(tool.wasi.env, vec!["FOO=bar"]);
        assert_eq!(tool.wasi.fs.len(), 1);
        assert_eq!(tool.wasi.fs[0].host, "/tmp/data");
        assert_eq!(tool.wasi.fs[0].guest, "/data");
        assert_eq!(tool.wasi.fs[0].perms, "rw");
    }

    #[test]
    fn wasm_section_absent_defaults_to_none() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");
        assert!(config.wasm.is_none());
    }

    #[test]
    fn wasm_tool_defaults_fuel_zero_and_empty_wasi() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[wasm.tools]]
name = "minimal"
module = "tool.wasm"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");
        let wasm = config.wasm.expect("wasm section");
        let tool = &wasm.tools[0];
        assert_eq!(tool.fuel, 0);
        assert!(tool.wasi.fs.is_empty());
        assert!(tool.wasi.env.is_empty());
    }

    #[test]
    fn wasm_mount_defaults_perms_to_ro() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[wasm.tools]]
name = "t"
module = "t.wasm"

[[wasm.tools.wasi.fs]]
host = "/src"
guest = "/src"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");
        let mount = &config.wasm.unwrap().tools[0].wasi.fs[0];
        assert_eq!(mount.perms, "ro", "default perms should be read-only");
    }

    // ── Hooks config ──────────────────────────────────────────────────

    #[test]
    fn hooks_section_parsed() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[hooks.tool_call]]
name = "audit-tools"
runtime = "js"
module = "./hooks/audit.js"

[[hooks.llm]]
name = "rate-limiter"
runtime = "js"
module = "./hooks/rate_limit.js"
timeout_ms = 200

[[hooks.spawn]]
name = "spawn-guard"
runtime = "js"
module = "./hooks/spawn_guard.js"

[[hooks.http_request]]
name = "url-filter"
runtime = "js"
module = "./hooks/url_filter.js"

[[hooks.vfs_write]]
name = "vfs-audit"
runtime = "js"
module = "./hooks/vfs_audit.js"
timeout_ms = 50
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse hooks config");

        let hooks = config.hooks.expect("hooks section should be present");
        assert_eq!(hooks.tool_call.len(), 1);
        assert_eq!(hooks.tool_call[0].name, "audit-tools");
        assert_eq!(hooks.tool_call[0].runtime, "js");
        assert_eq!(hooks.tool_call[0].module, "./hooks/audit.js");
        assert_eq!(
            hooks.tool_call[0].timeout_ms, 100,
            "default timeout should be 100ms"
        );

        assert_eq!(hooks.llm.len(), 1);
        assert_eq!(hooks.llm[0].name, "rate-limiter");
        assert_eq!(
            hooks.llm[0].timeout_ms, 200,
            "explicit timeout should override default"
        );

        assert_eq!(hooks.spawn.len(), 1);
        assert_eq!(hooks.spawn[0].name, "spawn-guard");

        assert_eq!(hooks.http_request.len(), 1);
        assert_eq!(hooks.http_request[0].name, "url-filter");

        // S039: `[[hooks.vfs_write]]` is parsed alongside the other op chains.
        assert_eq!(hooks.vfs_write.len(), 1);
        assert_eq!(hooks.vfs_write[0].name, "vfs-audit");
        assert_eq!(hooks.vfs_write[0].runtime, "js");
        assert_eq!(hooks.vfs_write[0].module, "./hooks/vfs_audit.js");
        assert_eq!(
            hooks.vfs_write[0].timeout_ms, 50,
            "explicit vfs_write timeout should override default"
        );
    }

    /// S039 round-trip test: an `[[hooks.vfs_write]]` entry must parse back
    /// out to itself. Pins the field name in the TOML surface.
    #[test]
    fn hooks_vfs_write_round_trips_through_toml() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[hooks.vfs_write]]
name = "vfs-audit"
runtime = "js"
module = "./hooks/vfs_audit.js"
"#;
        let config: SimulacraConfig =
            toml::from_str(toml_str).expect("should parse vfs_write hook");
        let hooks = config.hooks.expect("hooks section should be present");
        assert_eq!(hooks.vfs_write.len(), 1);
        assert_eq!(hooks.vfs_write[0].name, "vfs-audit");
        assert_eq!(
            hooks.vfs_write[0].timeout_ms, 100,
            "default timeout should be 100ms when omitted"
        );
        // Defaults of other chains are empty.
        assert!(hooks.tool_call.is_empty());
        assert!(hooks.llm.is_empty());
        assert!(hooks.spawn.is_empty());
        assert!(hooks.http_request.is_empty());
    }

    #[test]
    fn hooks_section_absent_defaults_to_none() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");
        assert!(config.hooks.is_none());
    }

    // ── Validation (BLOCKER/WARNING fixes) ─────────────────────────────

    #[test]
    fn from_file_rejects_unknown_hook_runtime() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join("simulacra_config_test_bad_hook_runtime.toml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(
                br#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[hooks.tool_call]]
name = "bad"
runtime = "jss"
module = "./bad.js"
"#,
            )
            .unwrap();
        }

        let result = SimulacraConfig::from_file(path.to_str().unwrap());
        assert!(
            matches!(result, Err(ConfigError::Validation(ref msg)) if msg.contains("runtime")),
            "expected Validation error about runtime, got {:?}",
            result.as_ref().err()
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_accepts_known_hook_runtimes() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[hooks.tool_call]]
name = "ok"
runtime = "js"
module = "./ok.js"
"#;
        let config: SimulacraConfig = toml::from_str(toml_str).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_unknown_mcp_transport() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[mcp.servers]]
name = "s"
transport = "gopher"
url = "http://localhost:3000"
"#;
        let config: SimulacraConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, ConfigError::Validation(_)),
            "expected Validation error, got {}",
            msg
        );
        assert!(
            msg.contains("gopher"),
            "error should mention bad transport: {}",
            msg
        );
    }

    #[test]
    fn validate_accepts_known_mcp_transports() {
        for transport in ["sse", "http", "auto"] {
            let toml_str = format!(
                r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[mcp.servers]]
name = "s"
transport = "{transport}"
url = "http://localhost:3000"
"#
            );
            let config: SimulacraConfig = toml::from_str(&toml_str).unwrap();
            assert!(
                config.validate().is_ok(),
                "transport {transport} should be accepted"
            );
        }
    }

    #[test]
    fn validate_rejects_duplicate_mcp_server_names() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[[mcp.servers]]
name = "github"
url = "http://localhost:3000"

[[mcp.servers]]
name = "github"
url = "http://localhost:3001"
"#;
        let config: SimulacraConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, ConfigError::Validation(_)),
            "expected Validation error, got {}",
            msg
        );
        assert!(
            msg.to_lowercase().contains("duplicate"),
            "error should mention duplicate: {}",
            msg
        );
    }

    #[test]
    fn hooks_section_present_but_empty() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[hooks]
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");
        let hooks = config.hooks.expect("hooks section should be present");
        assert!(hooks.tool_call.is_empty());
        assert!(hooks.llm.is_empty());
        assert!(hooks.spawn.is_empty());
        assert!(hooks.http_request.is_empty());
        // S039: vfs_write also defaults to empty.
        assert!(hooks.vfs_write.is_empty());
    }
}

#[cfg(test)]
mod s038_memory_config {
    use super::*;
    use std::path::PathBuf;

    // S038 config parsing AC1: simulacra-config exposes a MemoryConfig that round-trips through TOML.
    #[test]
    fn memory_config_round_trips_through_toml() {
        let toml_str = r#"
dir = "./.x"
tenant = "cli"
"#;

        let memory: MemoryConfig = toml::from_str(toml_str).expect("should parse memory config");
        let serialized = toml::to_string(&memory).expect("should serialize memory config");
        let reparsed: MemoryConfig =
            toml::from_str(&serialized).expect("should parse serialized memory config");

        assert_eq!(reparsed.dir, PathBuf::from("./.x"));
        assert_eq!(reparsed.tenant, "cli");
    }

    // S038 config parsing AC2: MemoryConfig.tenant defaults to "cli" when omitted.
    #[test]
    fn memory_config_defaults_tenant_to_cli_when_absent() {
        let toml_str = r#"
dir = "./.x"
"#;

        let memory: MemoryConfig = toml::from_str(toml_str).expect("should parse memory config");

        assert_eq!(memory.dir, PathBuf::from("./.x"));
        assert_eq!(memory.tenant, "cli");
    }

    // S038 config parsing AC3: SimulacraConfig exposes an optional memory field with serde defaulting.
    #[test]
    fn simulacra_config_memory_field_defaults_during_deserialization() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");
        let memory: Option<MemoryConfig> = config.memory.clone();

        assert!(
            memory.is_none(),
            "memory should default to None when omitted"
        );
    }

    // S038 config parsing AC4: Parsing simulacra.toml with no [memory] section yields SimulacraConfig.memory == None.
    #[test]
    fn simulacra_config_without_memory_section_has_no_memory_config() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");

        assert!(
            config.memory.is_none(),
            "memory should be None when section is absent"
        );
    }

    // S038 config parsing AC5: Parsing [memory] dir = "./.x" yields Some(MemoryConfig { dir, tenant: "cli" }).
    #[test]
    fn simulacra_config_memory_section_uses_cli_tenant_default() {
        let toml_str = r#"
[project]
name = "test"

[agent_types.worker]
model = "m"

[memory]
dir = "./.x"
"#;

        let config: SimulacraConfig = toml::from_str(toml_str).expect("should parse");
        let memory = config.memory.expect("memory section should be present");

        assert_eq!(memory.dir, PathBuf::from("./.x"));
        assert_eq!(memory.tenant, "cli");
    }

    #[test]
    fn memory_on_model_change_round_trips_all_variants() {
        for (variant, expected_toml) in [
            (OnModelChange::Refuse, "refuse"),
            (OnModelChange::ReindexBackground, "reindex_background"),
            (OnModelChange::WipeAndRebuild, "wipe_and_rebuild"),
        ] {
            let memory = MemoryConfig {
                dir: PathBuf::from("./.x"),
                tenant: "cli".to_string(),
                retention: None,
                on_model_change: variant.clone(),
            };

            let serialized = toml::to_string(&memory).expect("should serialize memory config");
            assert!(
                serialized.contains(&format!("on_model_change = \"{expected_toml}\"")),
                "serialized TOML should contain snake_case on_model_change variant, got: {serialized}"
            );

            let reparsed: MemoryConfig =
                toml::from_str(&serialized).expect("should parse serialized memory config");
            assert_eq!(reparsed.on_model_change, variant);
        }
    }

    #[test]
    fn memory_on_model_change_defaults_to_refuse_when_omitted() {
        let toml_str = r#"
dir = "./.x"
tenant = "cli"
"#;

        let memory: MemoryConfig = toml::from_str(toml_str).expect("should parse memory config");

        assert_eq!(memory.dir, PathBuf::from("./.x"));
        assert_eq!(memory.tenant, "cli");
        assert_eq!(memory.on_model_change, OnModelChange::Refuse);
    }
}
