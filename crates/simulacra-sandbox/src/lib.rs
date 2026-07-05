//! Simulacra sandbox crate.
//!
//! Composes VFS + shell + QuickJS into an [`AgentCell`] with capability-gated
//! execution. All side-effecting operations are checked against the agent's
//! [`CapabilityToken`] before execution. Every operation follows the Golden Rule
//! sequence: span → capability check → budget check → journal → execute → return.

pub mod executor;
mod fetch_proxy;
mod file_io;
mod fs_proxy;
mod guards;
mod http;
mod js;
mod module_fetcher;
mod python;
mod runtime;
mod shell;
mod shell_http_proxy;
mod vfs_mutation;
mod vfs_state;

pub use executor::ScriptExecutor;
pub use fetch_proxy::AgentCellFetchProxy;
pub use shell_http_proxy::AgentCellShellHttpProxy;
pub use simulacra_http::HttpResponse;
pub use vfs_mutation::{VfsMutation, VfsWritePrecondition};

use guards::{check_and_journal_capability, journal_budget_exhaustion};
use runtime::SendableJsRuntime;
use simulacra_types::{
    AgentId, BudgetExhausted, CapabilityDenied, CapabilityToken, FsMetadata, ResourceBudget,
    VfsError, VirtualFs,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ── Capability denial attribution ────────────────────────────────
//
// Memory paths (`/var/memory/**` and `/mnt/**`) are gated by `MemoryCapability`,
// not by the generic `paths_read`/`paths_write` globs (S037 §11/§14). When a
// memory path is denied, the metrics counter must label the operation as a
// memory denial so operators can filter
// `simulacra.sandbox.capability.denials{operation="memory_search_scopes"}`
// separately from generic-glob denials. Without this, every demo that gates
// memory access would show denials attributed to `paths_write`, masking the
// real cause.

pub(crate) fn cap_name_for_read(path: &str) -> &'static str {
    if simulacra_types::MemoryPath::is_memory_path_str(path) {
        "memory_search_scopes"
    } else {
        "paths_read"
    }
}

pub(crate) fn cap_name_for_write(path: &str) -> &'static str {
    if simulacra_types::MemoryPath::is_memory_path_str(path) {
        "memory_write_scopes"
    } else {
        "paths_write"
    }
}

/// A sandboxed execution environment for a single agent.
///
/// Holds references to the virtual filesystem, capability token, resource budget,
/// and journal storage. All operations check capabilities and budget before execution.
pub struct AgentCell {
    pub(crate) vfs: Arc<dyn VirtualFs>,
    pub capability: CapabilityToken,
    pub(crate) budget: Arc<Mutex<ResourceBudget>>,
    pub(crate) journal: Arc<dyn simulacra_types::JournalStorage>,
    pub(crate) agent_id: AgentId,
    pub(crate) http_client: Arc<dyn simulacra_http::HttpClient>,
    /// Pre-registered module source stubs keyed by URL.
    /// When a remote module import matches a key, the stub source is returned
    /// instead of performing an HTTP fetch. This enables testing without a
    /// live HTTP server.
    pub(crate) module_stubs: Mutex<HashMap<String, String>>,
    /// Persistent shell environment variables, surviving across `execute_shell` calls.
    pub(crate) shell_env: Mutex<HashMap<String, String>>,
    /// Persistent shell working directory, surviving across `execute_shell` calls.
    /// `cd /tmp` in one call leaves the next call rooted at `/tmp`.
    pub(crate) shell_cwd: Mutex<String>,
    /// Serializes multi-step VFS mutation batches through this cell.
    pub(crate) vfs_mutation_lock: Mutex<()>,
    /// JS runtime wrapper that preserves mediated host configuration and
    /// remote source caches. Each eval creates a fresh QuickJS context.
    pub(crate) js_runtime: SendableJsRuntime,
    /// Optional bounded executor for script concurrency control.
    /// When present, `execute_js` acquires a permit before running. Direct sync
    /// callers use a non-blocking permit check; async callers should use
    /// `execute_js_async` so the permit is awaited once at the cell boundary.
    pub(crate) script_executor: Option<ScriptExecutor>,
    /// S033: Integration registry for credential injection into fetch().
    pub integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
    /// S033: Which integrations this agent's tenant is granted access to.
    pub tenant_integrations: Vec<String>,
}

impl AgentCell {
    /// Create a new `AgentCell` with full composition: VFS, capability, budget, journal, and HTTP client.
    pub fn new(
        vfs: Arc<dyn VirtualFs>,
        capability: CapabilityToken,
        budget: Arc<Mutex<ResourceBudget>>,
        journal: Arc<dyn simulacra_types::JournalStorage>,
        http_client: Arc<dyn simulacra_http::HttpClient>,
    ) -> Self {
        Self {
            vfs,
            capability,
            budget,
            journal,
            agent_id: AgentId("sandbox".into()),
            http_client,
            module_stubs: Mutex::new(HashMap::new()),
            shell_env: Mutex::new(HashMap::new()),
            shell_cwd: Mutex::new("/".to_string()),
            vfs_mutation_lock: Mutex::new(()),
            js_runtime: SendableJsRuntime::new(),
            script_executor: None,
            integration_registry: None,
            tenant_integrations: vec![],
        }
    }

    /// Set the script executor for bounded concurrency control.
    ///
    /// When set, JS execution acquires a permit before running (backpressure).
    /// Python and WASM tools use [`ScriptExecutor::execute`] for full
    /// `spawn_blocking` + backpressure.
    pub fn set_script_executor(&mut self, executor: ScriptExecutor) {
        self.script_executor = Some(executor);
    }

    /// Get the script executor, if one has been configured.
    pub fn script_executor(&self) -> Option<&ScriptExecutor> {
        self.script_executor.as_ref()
    }

    /// Register a module source stub for a given URL.
    ///
    /// When `execute_js` encounters an `import` from this URL, the stub source
    /// is used instead of performing an HTTP fetch. The fetch still goes through
    /// the full Golden Rule chain (capability check, budget, journal, span).
    pub fn register_module_stub(&self, url: &str, source: &str) {
        self.module_stubs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(url.to_string(), source.to_string());
    }

    /// Read a file from the VFS, checking path read capability first.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, SandboxError> {
        file_io::read_file_inner(
            path,
            &self.vfs,
            &self.capability,
            &self.budget,
            &self.journal,
            &self.agent_id,
        )
    }

    /// Write a file to the VFS, checking path write capability and VFS bytes budget.
    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<(), SandboxError> {
        file_io::write_file_inner(
            path,
            data,
            &self.vfs,
            &self.capability,
            &self.budget,
            &self.journal,
            &self.agent_id,
        )
    }

    /// Check write admission for a future VFS mutation without mutating the VFS.
    pub fn preflight_path_write(&self, path: &str) -> Result<(), SandboxError> {
        check_and_journal_capability(
            || self.capability.check_path_write(path),
            "preflight_path_write",
            cap_name_for_write(path),
            &self.journal,
            &self.agent_id,
        )
    }

    /// Check whether a path exists when the caller has write capability for it.
    pub fn path_exists_for_write(&self, path: &str) -> Result<bool, SandboxError> {
        let _span = tracing::info_span!(
            "sandbox_path_exists_for_write",
            simulacra.operation.name = "sandbox_path_exists_for_write",
            simulacra.vfs.path = path,
        )
        .entered();

        check_and_journal_capability(
            || self.capability.check_path_write(path),
            "path_exists_for_write",
            cap_name_for_write(path),
            &self.journal,
            &self.agent_id,
        )?;

        Ok(self.vfs.exists(path))
    }

    /// Check whether a future batch of VFS writes fits the current byte budget.
    pub fn preflight_vfs_write_bytes(&self, bytes: u64) -> Result<(), SandboxError> {
        let b = self
            .budget
            .lock()
            .map_err(|e| SandboxError::Internal(format!("budget mutex poisoned: {e}")))?;
        if let Err(exhausted) = b.check_budget() {
            journal_budget_exhaustion(&self.journal, &self.agent_id, &exhausted);
            tracing::warn!(
                simulacra.budget.resource = %exhausted.resource,
                simulacra.budget.used = %exhausted.used,
                simulacra.budget.limit = %exhausted.limit,
                "budget exhausted"
            );
            return Err(SandboxError::BudgetExhausted(exhausted));
        }

        let projected = b.used_vfs_bytes.saturating_add(bytes);
        if b.max_vfs_bytes > 0 && projected > b.max_vfs_bytes {
            let exhausted = BudgetExhausted {
                resource: "vfs_bytes".into(),
                used: projected.to_string(),
                limit: b.max_vfs_bytes.to_string(),
            };
            journal_budget_exhaustion(&self.journal, &self.agent_id, &exhausted);
            tracing::warn!(
                simulacra.budget.resource = "vfs_bytes",
                simulacra.budget.used = %projected,
                simulacra.budget.limit = %b.max_vfs_bytes,
                "budget exhausted"
            );
            return Err(SandboxError::BudgetExhausted(exhausted));
        }

        Ok(())
    }

    /// List directory contents, checking path read capability.
    pub fn list_dir(&self, path: &str) -> Result<Vec<String>, SandboxError> {
        let _span = tracing::info_span!(
            "sandbox_list_dir",
            simulacra.operation.name = "sandbox_list_dir",
            simulacra.vfs.path = path,
        )
        .entered();

        check_and_journal_capability(
            || self.capability.check_path_read(path),
            "list_dir",
            cap_name_for_read(path),
            &self.journal,
            &self.agent_id,
        )?;

        // No budget check — S011 §10: list_dir is a metadata query, not a tool invocation.
        let entries = self.vfs.list_dir(path).map_err(SandboxError::Vfs)?;

        // S029 §72: /proc list_dir produces a journal entry (same as regular file reads).
        if path == "/proc" || path.starts_with("/proc/") {
            let append_result = self.journal.append(simulacra_types::JournalEntry {
                schema_version: simulacra_types::JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: simulacra_types::JournalEntryKind::ToolResult {
                    tool_call_id: None,

                    tool_name: "list_dir".to_string(),
                    content: format!("listed {} entries in {}", entries.len(), path),
                    is_error: false,
                },
            });
            if let Err(err) = append_result {
                tracing::error!(error = %err, "journal append failed for list_dir");
            }
        }

        Ok(entries)
    }

    /// Return VFS metadata, checking path read capability first.
    pub fn metadata(&self, path: &str) -> Result<FsMetadata, SandboxError> {
        let _span = tracing::info_span!(
            "sandbox_metadata",
            simulacra.operation.name = "sandbox_metadata",
            simulacra.vfs.path = path,
        )
        .entered();

        check_and_journal_capability(
            || {
                self.capability.check_path_read(path).map_err(|mut denied| {
                    denied.operation = "path_read".into();
                    denied
                })
            },
            "metadata",
            cap_name_for_read(path),
            &self.journal,
            &self.agent_id,
        )?;

        self.vfs.metadata(path).map_err(SandboxError::Vfs)
    }

    /// Make an HTTP request, checking network capability and turns budget.
    ///
    /// Follows the Golden Rule: capability check → budget check → increment turns →
    /// OTel span → execute → journal → return.
    pub fn fetch_http(
        &self,
        url: &str,
        method: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        timeout_ms: Option<u64>,
    ) -> Result<HttpResponse, SandboxError> {
        http::fetch_http_inner(
            url,
            method,
            headers,
            body,
            &self.capability,
            &self.budget,
            &self.journal,
            &self.agent_id,
            true,
            "fetch_http",
            &*self.http_client,
            timeout_ms,
        )
    }
}

/// Errors from sandbox operations.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("capability denied: {0}")]
    CapabilityDenied(#[from] CapabilityDenied),
    #[error("budget exhausted: {0}")]
    BudgetExhausted(#[from] BudgetExhausted),
    #[error("shell error: {0}")]
    Shell(String),
    #[error("http error: {0}")]
    Http(String),
    #[error("js error: {0}")]
    Js(String),
    #[error("vfs error: {0}")]
    Vfs(VfsError),
    #[error("internal error: {0}")]
    Internal(String),
}

#[cfg(test)]
#[path = "unit_tests.rs"]
mod tests;
