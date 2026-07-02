//! Simulacra sandbox crate.
//!
//! Composes VFS + shell + QuickJS into an [`AgentCell`] with capability-gated execution.
//! All side-effecting operations are checked against the agent's [`CapabilityToken`]
//! before execution. Every operation follows the Golden Rule sequence:
//! span → capability check → budget check → journal → execute → return.

pub mod executor;
mod fetch_proxy;
mod fs_proxy;
mod guards;
mod http;
mod module_fetcher;
mod shell_http_proxy;

pub use executor::ScriptExecutor;
pub use fetch_proxy::AgentCellFetchProxy;
pub use shell_http_proxy::AgentCellShellHttpProxy;
pub use simulacra_http::HttpResponse;

use fs_proxy::AgentCellFsProxy;
use guards::{
    check_and_journal_capability, journal_budget_exhaustion, release_vfs_bytes, reserve_turn,
    reserve_vfs_bytes,
};
use module_fetcher::AgentCellModuleFetcher;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use simulacra_quickjs::{FsProxy, JsOutput, JsRuntime, ModuleFetcher};
use simulacra_types::{
    AgentId, BudgetExhausted, CapabilityDenied, CapabilityToken, FsMetadata,
    JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalStorage, ResourceBudget,
    VfsError, VirtualFs,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

// ── Capability denial attribution ────────────────────────────────
//
// Memory paths (`/var/memory/**` and `/mnt/**`) are gated by `MemoryCapability`,
// not by the generic `paths_read`/`paths_write` globs (S037 §11/§14). When a
// memory path is denied, the metrics counter must label the operation as a
// memory denial so operators can filter `simulacra.sandbox.capability.denials{
// operation="memory_search_scopes"}` separately from generic-glob denials.
// Without this, every demo that gates memory access would show denials
// attributed to `paths_write`, masking the real cause.

fn cap_name_for_read(path: &str) -> &'static str {
    if simulacra_types::MemoryPath::is_memory_path_str(path) {
        "memory_search_scopes"
    } else {
        "paths_read"
    }
}

fn cap_name_for_write(path: &str) -> &'static str {
    if simulacra_types::MemoryPath::is_memory_path_str(path) {
        "memory_write_scopes"
    } else {
        "paths_write"
    }
}

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for the sandbox.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
struct SandboxMeters {
    shell_duration: Histogram<f64>,
    shell_requests: Counter<u64>,
    js_duration: Histogram<f64>,
    js_requests: Counter<u64>,
}

impl SandboxMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<SandboxMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-sandbox");
            SandboxMeters {
                shell_duration: meter
                    .f64_histogram("simulacra.sandbox.shell.duration")
                    .with_unit("ms")
                    .with_description("Shell command execution duration")
                    .build(),
                shell_requests: meter
                    .u64_counter("simulacra.sandbox.shell.requests")
                    .with_description("Total shell command executions")
                    .build(),
                js_duration: meter
                    .f64_histogram("simulacra.sandbox.js.duration")
                    .with_unit("ms")
                    .with_description("JavaScript execution duration")
                    .build(),
                js_requests: meter
                    .u64_counter("simulacra.sandbox.js.requests")
                    .with_description("Total JavaScript executions")
                    .build(),
            }
        })
    }
}

/// Wrapper around [`JsRuntime`] that implements `Send` and `Sync`.
///
/// QuickJS contexts are not `Send`/`Sync` because they contain `Rc` and raw
/// pointers. However, `AgentCell` is designed so that each cell is owned
/// exclusively by a single agent task. The `Sync` bound is required because
/// `Arc<AgentCell>` is used in `simulacra-tool`, but concurrent access to the
/// JS runtime never actually occurs — each tool invocation runs sequentially
/// on the owning task. The inner `Mutex` provides runtime protection against
/// accidental concurrent access, replacing the previous `UnsafeCell`.
struct SendableJsRuntime(Mutex<Option<JsRuntime>>);

// SAFETY: JsRuntime is !Send because rquickjs types contain Rc and raw pointers.
// However, AgentCell ensures the runtime is only ever used from one logical task.
// The Mutex provides runtime synchronization so concurrent access is impossible.
// We assert Send so that AgentCell (which holds this field) can be moved between
// threads and placed in an Arc.
//
// Thread-affinity analysis: QuickJS has no thread-affinity requirement — it does
// not use thread-local storage or thread-pinned resources. The `!Send` bound on
// rquickjs types is a conservative Rust-side restriction due to internal `Rc`s.
// Our `Mutex<Option<JsRuntime>>` serializes all access, so only one thread ever
// touches the runtime at a time. The `ScriptExecutor::acquire_permit()` path
// (used for JS) runs the runtime inline on whatever thread holds the Mutex lock
// — this is safe because (a) the Mutex prevents concurrent access, and (b) QuickJS
// has no requirement to run on a specific thread, only that it not be accessed
// concurrently.
unsafe impl Send for SendableJsRuntime {}
unsafe impl Sync for SendableJsRuntime {}

impl SendableJsRuntime {
    fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Lock and return a guard to the inner option.
    fn lock(&self) -> Result<MutexGuard<'_, Option<JsRuntime>>, SandboxError> {
        self.0
            .lock()
            .map_err(|e| SandboxError::Internal(format!("js runtime mutex poisoned: {e}")))
    }
}

/// A sandboxed execution environment for a single agent.
///
/// Holds references to the virtual filesystem, capability token, resource budget,
/// and journal storage. All operations check capabilities and budget before execution.
pub struct AgentCell {
    vfs: Arc<dyn VirtualFs>,
    pub capability: CapabilityToken,
    budget: Arc<Mutex<ResourceBudget>>,
    journal: Arc<dyn JournalStorage>,
    agent_id: AgentId,
    http_client: Arc<dyn simulacra_http::HttpClient>,
    /// Pre-registered module source stubs keyed by URL.
    /// When a remote module import matches a key, the stub source is returned
    /// instead of performing an HTTP fetch. This enables testing without a
    /// live HTTP server.
    module_stubs: Mutex<HashMap<String, String>>,
    /// Persistent shell environment variables, surviving across `execute_shell` calls.
    shell_env: Mutex<HashMap<String, String>>,
    /// Persistent shell working directory, surviving across `execute_shell` calls.
    /// `cd /tmp` in one call leaves the next call rooted at `/tmp`.
    shell_cwd: Mutex<String>,
    /// Serializes multi-step VFS mutation batches through this cell.
    vfs_mutation_lock: Mutex<()>,
    /// JS runtime wrapper that preserves mediated host configuration and
    /// remote source caches. Each eval creates a fresh QuickJS context.
    js_runtime: SendableJsRuntime,
    /// Optional bounded executor for script concurrency control.
    /// When present, `execute_js` acquires a permit before running. JS runtime
    /// execution is synchronous and `!Send`, so direct `execute_js` callers use
    /// a non-blocking permit check while async tool wrappers can await.
    script_executor: Option<ScriptExecutor>,
    /// S033: Integration registry for credential injection into fetch().
    pub integration_registry: Option<Arc<simulacra_integration::IntegrationRegistry>>,
    /// S033: Which integrations this agent's tenant is granted access to.
    pub tenant_integrations: Vec<String>,
}

/// A VFS mutation batch item executed through [`AgentCell`].
#[derive(Debug, Clone)]
pub enum VfsMutation {
    Write {
        path: String,
        data: Vec<u8>,
        precondition: VfsWritePrecondition,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    MoveAndWrite {
        from: String,
        to: String,
        data: Vec<u8>,
        from_precondition: Option<Vec<u8>>,
    },
}

/// Expected state for a [`VfsMutation::Write`] before it may execute.
#[derive(Debug, Clone)]
pub enum VfsWritePrecondition {
    Any,
    Missing,
    Matches(Vec<u8>),
}

#[derive(Debug, Clone)]
enum VfsPathState {
    Missing,
    File(Vec<u8>),
    Dir,
}

#[derive(Debug, Clone)]
struct VfsRollbackExpectation {
    path: String,
    expected_after: VfsPathState,
}

#[derive(Debug, Clone)]
struct VfsRollbackEntry {
    path: String,
    before: VfsPathState,
    expected_after: VfsPathState,
}

#[derive(Debug, Clone)]
enum PreparedVfsMutation {
    Write {
        path: String,
        data: Vec<u8>,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
        data: Vec<u8>,
    },
    MoveAndWrite {
        from: String,
        to: String,
        data: Vec<u8>,
    },
}

impl AgentCell {
    /// Create a new `AgentCell` with full composition: VFS, capability, budget, journal, and HTTP client.
    pub fn new(
        vfs: Arc<dyn VirtualFs>,
        capability: CapabilityToken,
        budget: Arc<Mutex<ResourceBudget>>,
        journal: Arc<dyn JournalStorage>,
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
        read_file_inner(
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
        write_file_inner(
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

    /// Apply a set of VFS mutations as a single capability-checked, journaled batch.
    pub fn apply_vfs_mutations(
        &self,
        tool_name: &str,
        mutations: &[VfsMutation],
    ) -> Result<(), SandboxError> {
        let _span = tracing::info_span!(
            "sandbox_apply_vfs_mutations",
            simulacra.operation.name = "sandbox_apply_vfs_mutations",
            simulacra.tool.name = tool_name,
            simulacra.vfs.mutation_count = mutations.len() as u64,
        )
        .entered();

        let _mutation_guard = self
            .vfs_mutation_lock
            .lock()
            .map_err(|e| SandboxError::Internal(format!("vfs mutation mutex poisoned: {e}")))?;

        for mutation in mutations {
            match mutation {
                VfsMutation::Write { path, .. } | VfsMutation::Delete { path } => {
                    check_and_journal_capability(
                        || self.capability.check_path_write(path),
                        tool_name,
                        cap_name_for_write(path),
                        &self.journal,
                        &self.agent_id,
                    )?;
                }
                VfsMutation::Move { from, to } | VfsMutation::MoveAndWrite { from, to, .. } => {
                    check_and_journal_capability(
                        || self.capability.check_path_read(from),
                        tool_name,
                        cap_name_for_read(from),
                        &self.journal,
                        &self.agent_id,
                    )?;
                    check_and_journal_capability(
                        || self.capability.check_path_write(from),
                        tool_name,
                        cap_name_for_write(from),
                        &self.journal,
                        &self.agent_id,
                    )?;
                    check_and_journal_capability(
                        || self.capability.check_path_write(to),
                        tool_name,
                        cap_name_for_write(to),
                        &self.journal,
                        &self.agent_id,
                    )?;
                }
            }
        }

        self.preflight_vfs_write_bytes(0)?;

        let mut write_bytes = 0_u64;
        let mut file_write_entries = Vec::new();
        let mut file_delete_entries = Vec::new();
        let mut file_move_entries = Vec::new();
        for mutation in mutations {
            match mutation {
                VfsMutation::Write { path, data, .. } => {
                    write_bytes = write_bytes.saturating_add(data.len() as u64);
                    file_write_entries.push((path.as_str(), data.len() as u64));
                }
                VfsMutation::Delete { path } => {
                    file_delete_entries.push(path.as_str());
                }
                VfsMutation::Move { from, to } => {
                    let metadata = self.vfs.metadata(from).map_err(SandboxError::Vfs)?;
                    if !metadata.is_file {
                        return Err(SandboxError::Vfs(VfsError::NotAFile(from.clone())));
                    }
                    write_bytes = write_bytes.saturating_add(metadata.size);
                    file_move_entries.push((from.as_str(), to.as_str()));
                    file_write_entries.push((to.as_str(), metadata.size));
                }
                VfsMutation::MoveAndWrite { from, to, data, .. } => {
                    write_bytes = write_bytes.saturating_add(data.len() as u64);
                    file_move_entries.push((from.as_str(), to.as_str()));
                    file_write_entries.push((to.as_str(), data.len() as u64));
                }
            }
        }

        if write_bytes > 0 {
            reserve_vfs_bytes(&self.budget, write_bytes, &self.journal, &self.agent_id)?;
        }

        let mut rollback_expectations = Vec::new();
        let mut created_parent_dirs = Vec::new();
        let mut prepared_mutations = Vec::with_capacity(mutations.len());
        let preparation_result: Result<(), SandboxError> = (|| {
            for mutation in mutations {
                match mutation {
                    VfsMutation::Write {
                        path,
                        data,
                        precondition,
                    } => {
                        ensure_parent_components_are_directories(&self.vfs, path)?;
                        validate_write_precondition(&self.vfs, path, precondition)?;
                        upsert_rollback_expectation(
                            &mut rollback_expectations,
                            path,
                            VfsPathState::File(data.clone()),
                        );
                        record_missing_parent_dirs(&self.vfs, path, &mut created_parent_dirs);
                        prepared_mutations.push(PreparedVfsMutation::Write {
                            path: path.clone(),
                            data: data.clone(),
                        });
                    }
                    VfsMutation::Delete { path } => {
                        let metadata = self.vfs.metadata(path).map_err(SandboxError::Vfs)?;
                        if !metadata.is_file {
                            return Err(SandboxError::Vfs(VfsError::NotAFile(path.clone())));
                        }
                        upsert_rollback_expectation(
                            &mut rollback_expectations,
                            path,
                            VfsPathState::Missing,
                        );
                        prepared_mutations.push(PreparedVfsMutation::Delete { path: path.clone() });
                    }
                    VfsMutation::Move { from, to } => {
                        let metadata = self.vfs.metadata(from).map_err(SandboxError::Vfs)?;
                        if !metadata.is_file {
                            return Err(SandboxError::Vfs(VfsError::NotAFile(from.clone())));
                        }
                        ensure_parent_components_are_directories(&self.vfs, to)?;
                        ensure_vfs_path_missing(&self.vfs, to)?;
                        let data = self.vfs.read(from).map_err(SandboxError::Vfs)?;
                        upsert_rollback_expectation(
                            &mut rollback_expectations,
                            from,
                            VfsPathState::Missing,
                        );
                        upsert_rollback_expectation(
                            &mut rollback_expectations,
                            to,
                            VfsPathState::File(data.clone()),
                        );
                        record_missing_parent_dirs(&self.vfs, to, &mut created_parent_dirs);
                        prepared_mutations.push(PreparedVfsMutation::Move {
                            from: from.clone(),
                            to: to.clone(),
                            data,
                        });
                    }
                    VfsMutation::MoveAndWrite {
                        from,
                        to,
                        data,
                        from_precondition,
                    } => {
                        let metadata = self.vfs.metadata(from).map_err(SandboxError::Vfs)?;
                        if !metadata.is_file {
                            return Err(SandboxError::Vfs(VfsError::NotAFile(from.clone())));
                        }
                        if let Some(expected) = from_precondition {
                            validate_file_matches(&self.vfs, from, expected)?;
                        }
                        ensure_parent_components_are_directories(&self.vfs, to)?;
                        ensure_vfs_path_missing(&self.vfs, to)?;
                        upsert_rollback_expectation(
                            &mut rollback_expectations,
                            from,
                            VfsPathState::Missing,
                        );
                        upsert_rollback_expectation(
                            &mut rollback_expectations,
                            to,
                            VfsPathState::File(data.clone()),
                        );
                        record_missing_parent_dirs(&self.vfs, to, &mut created_parent_dirs);
                        prepared_mutations.push(PreparedVfsMutation::MoveAndWrite {
                            from: from.clone(),
                            to: to.clone(),
                            data: data.clone(),
                        });
                    }
                }
            }
            Ok(())
        })();

        if let Err(err) = preparation_result {
            if write_bytes > 0 {
                release_vfs_bytes(&self.budget, write_bytes)?;
            }
            return Err(err);
        }

        let rollback_states = match capture_vfs_rollback_entries(&self.vfs, &rollback_expectations)
        {
            Ok(states) => states,
            Err(err) => {
                if write_bytes > 0 {
                    release_vfs_bytes(&self.budget, write_bytes)?;
                }
                return Err(SandboxError::Vfs(err));
            }
        };

        let append_plan_result = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::ToolResult {
                tool_call_id: None,
                tool_name: tool_name.to_string(),
                content: format!("planned {} VFS mutation(s)", mutations.len()),
                is_error: false,
            },
        });
        if let Err(err) = append_plan_result {
            if write_bytes > 0 {
                release_vfs_bytes(&self.budget, write_bytes)?;
            }
            tracing::error!(error = %err, "journal append failed for VFS mutation plan");
            return Err(SandboxError::Internal(format!(
                "journal append failed for VFS mutation plan: {err}"
            )));
        }

        for path in file_delete_entries {
            if let Err(err) = self.journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::FileDelete {
                    path: path.to_string(),
                },
            }) {
                if write_bytes > 0 {
                    release_vfs_bytes(&self.budget, write_bytes)?;
                }
                tracing::error!(error = %err, "journal append failed for VFS mutation delete");
                return Err(SandboxError::Internal(format!(
                    "journal append failed for VFS mutation delete: {err}"
                )));
            }
        }

        for (from, to) in file_move_entries {
            if let Err(err) = self.journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::FileMove {
                    from: from.to_string(),
                    to: to.to_string(),
                },
            }) {
                if write_bytes > 0 {
                    release_vfs_bytes(&self.budget, write_bytes)?;
                }
                tracing::error!(error = %err, "journal append failed for VFS mutation move");
                return Err(SandboxError::Internal(format!(
                    "journal append failed for VFS mutation move: {err}"
                )));
            }
        }

        for (path, size_bytes) in file_write_entries {
            if let Err(err) = self.journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::FileWrite {
                    path: path.to_string(),
                    size_bytes,
                },
            }) {
                if write_bytes > 0 {
                    release_vfs_bytes(&self.budget, write_bytes)?;
                }
                tracing::error!(error = %err, "journal append failed for VFS mutation write");
                return Err(SandboxError::Internal(format!(
                    "journal append failed for VFS mutation write: {err}"
                )));
            }
        }

        let result: Result<(), VfsError> = (|| {
            for mutation in &prepared_mutations {
                match mutation {
                    PreparedVfsMutation::Write { path, data } => {
                        self.vfs.write(path, data)?;
                    }
                    PreparedVfsMutation::Delete { path } => {
                        self.vfs.remove(path)?;
                    }
                    PreparedVfsMutation::Move { from, to, data } => {
                        self.vfs.write(to, data)?;
                        self.vfs.remove(from)?;
                    }
                    PreparedVfsMutation::MoveAndWrite { from, to, data } => {
                        self.vfs.write(to, data)?;
                        self.vfs.remove(from)?;
                    }
                }
            }
            Ok(())
        })();

        if let Err(err) = result {
            if write_bytes > 0 {
                release_vfs_bytes(&self.budget, write_bytes)?;
            }
            if let Err(restore_err) =
                restore_vfs_path_states(&self.vfs, &rollback_states, &created_parent_dirs)
            {
                return Err(SandboxError::Internal(format!(
                    "failed to roll back VFS mutations after {err}: {restore_err}"
                )));
            }
            if let Err(journal_err) = self.journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::ToolResult {
                    tool_call_id: None,
                    tool_name: tool_name.to_string(),
                    content: err.to_string(),
                    is_error: true,
                },
            }) {
                tracing::error!(error = %journal_err, "journal append failed for VFS mutation error");
            }
            return Err(SandboxError::Vfs(err));
        }

        Ok(())
    }

    /// Remove a VFS path, checking path write capability first.
    pub fn remove_path(&self, path: &str) -> Result<(), SandboxError> {
        self.apply_vfs_mutations(
            "remove_path",
            &[VfsMutation::Delete {
                path: path.to_string(),
            }],
        )
    }

    /// Move a VFS file, checking read capability for the source and write
    /// capability for both paths.
    pub fn move_path(&self, from: &str, to: &str) -> Result<(), SandboxError> {
        self.apply_vfs_mutations(
            "move_path",
            &[VfsMutation::Move {
                from: from.to_string(),
                to: to.to_string(),
            }],
        )
    }

    /// List directory contents, checking path read capability.
    pub fn list_dir(&self, path: &str) -> Result<Vec<String>, SandboxError> {
        let _span = tracing::info_span!(
            "sandbox_list_dir",
            simulacra.operation.name = "sandbox_list_dir",
            simulacra.vfs.path = path,
        )
        .entered();

        // Check capability
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
            let append_result = self.journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::ToolResult {
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

    fn record_shell_command(&self, command: &str, exit_code: i32) {
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::ShellCommand {
                command: command.to_string(),
                exit_code,
            },
        }) {
            tracing::error!(error = %err, "journal append failed for execute_shell");
        }
    }

    fn record_shell_meters(&self, shell_start: std::time::Instant) {
        let meters = SandboxMeters::get();
        let attrs = &[KeyValue::new("simulacra.agent.id", self.agent_id.0.clone())];
        meters
            .shell_duration
            .record(shell_start.elapsed().as_secs_f64() * 1000.0, attrs);
        meters.shell_requests.add(1, attrs);
    }

    fn finish_shell_command(
        &self,
        command: &str,
        shell_start: std::time::Instant,
        result: simulacra_shell::CommandResult,
    ) -> Result<simulacra_shell::CommandResult, SandboxError> {
        self.record_shell_command(command, result.exit_code);
        self.record_shell_meters(shell_start);
        Ok(result)
    }

    /// Reserve one execution turn and journal a top-level Python code execution.
    ///
    /// `py_exec` lives in `simulacra-python`, but its admission control belongs to
    /// the same AgentCell budget/journal stream as shell and JavaScript.
    pub fn begin_python_execution(&self) -> Result<(), SandboxError> {
        reserve_turn(&self.budget, &self.journal, &self.agent_id)?;
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::CodeExecution {
                language: "python".to_string(),
            },
        }) {
            tracing::error!(error = %err, "journal append failed for py_exec");
        }
        Ok(())
    }

    fn js_shell_result(&self, code: &str) -> simulacra_shell::CommandResult {
        match self.execute_js(code) {
            Ok(js_out) => {
                let mut stdout = js_out.stdout;
                if let Some(ref result) = js_out.result
                    && !result.is_empty()
                    && result != "undefined"
                {
                    stdout.push_str(result);
                    stdout.push('\n');
                }
                simulacra_shell::CommandResult {
                    stdout,
                    stderr: String::new(),
                    exit_code: js_out.exit_code.unwrap_or(0),
                }
            }
            Err(e) => simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: format!("{e}\n"),
                exit_code: 1,
            },
        }
    }

    #[cfg(feature = "python")]
    fn python_shell_result(&self, code: &str) -> simulacra_shell::CommandResult {
        let runtime = simulacra_python_runtime::PythonRuntime::new(
            simulacra_python_runtime::PythonResourceLimits {
                max_duration: Some(std::time::Duration::from_secs(30)),
                max_recursion_depth: Some(1000),
                ..simulacra_python_runtime::PythonResourceLimits::default()
            },
        );
        let dispatcher = PythonShellDispatcher { cell: self };
        match runtime.execute(code, &dispatcher) {
            Ok(output) => simulacra_shell::CommandResult {
                stdout: output.stdout,
                stderr: String::new(),
                exit_code: 0,
            },
            Err(e) => simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: format!("{e}\n"),
                exit_code: 1,
            },
        }
    }

    /// Execute a shell command, checking shell capability and turns budget.
    pub fn execute_shell(
        &self,
        command: &str,
    ) -> Result<simulacra_shell::CommandResult, SandboxError> {
        self.execute_shell_with_workdir(command, None)
    }

    /// Execute a shell command with an optional one-call working directory.
    pub fn execute_shell_with_workdir(
        &self,
        command: &str,
        workdir: Option<&str>,
    ) -> Result<simulacra_shell::CommandResult, SandboxError> {
        // Rebuild interest cache to ensure callsite is evaluated
        // against the current thread-local subscriber, not a stale
        // cached decision from a different thread.
        tracing::callsite::rebuild_interest_cache();
        let _span = tracing::info_span!(
            "sandbox_shell_exec",
            simulacra.operation.name = "sandbox_shell_exec",
            simulacra.shell.command = command,
        )
        .entered();

        // Check capability
        check_and_journal_capability(
            || self.capability.check_shell(),
            "execute_shell",
            "shell",
            &self.journal,
            &self.agent_id,
        )?;

        if let Some(path) = workdir {
            let metadata = self.metadata(path)?;
            if !metadata.is_dir {
                return Err(SandboxError::Vfs(VfsError::NotADirectory(path.to_string())));
            }
        }

        // Atomically reserve the turn before execution.
        reserve_turn(&self.budget, &self.journal, &self.agent_id)?;

        let shell_start = std::time::Instant::now();

        // Execute with persistent shell environment
        let env = self
            .shell_env
            .lock()
            .map_err(|e| SandboxError::Internal(format!("shell_env mutex poisoned: {e}")))?
            .clone();
        let shell_http_proxy = AgentCellShellHttpProxy {
            capability: self.capability.clone(),
            budget: Arc::clone(&self.budget),
            journal: Arc::clone(&self.journal),
            agent_id: self.agent_id.clone(),
            http_client: Arc::clone(&self.http_client),
        };
        let cwd = match workdir {
            Some(path) => path.to_string(),
            None => self
                .shell_cwd
                .lock()
                .map_err(|e| SandboxError::Internal(format!("shell_cwd mutex poisoned: {e}")))?
                .clone(),
        };
        let previous_cwd = self
            .shell_cwd
            .lock()
            .map_err(|e| SandboxError::Internal(format!("shell_cwd mutex poisoned: {e}")))?
            .clone();
        let shell_vfs = AgentCellFsProxy {
            vfs: Arc::clone(&self.vfs),
            capability: self.capability.clone(),
            budget: Arc::clone(&self.budget),
            journal: Arc::clone(&self.journal),
            agent_id: self.agent_id.clone(),
        };
        let shell_external = AgentCellShellExternal { cell: self };
        let executor =
            simulacra_shell::ShellExecutor::new(&shell_vfs, env, Some(&shell_http_proxy));
        let executor = if workdir.is_some() {
            executor.try_with_cwd(cwd).map_err(SandboxError::Vfs)?
        } else {
            executor.with_cwd(cwd)
        };
        let mut executor = executor.with_external(&shell_external);
        let result = executor.run(command);
        // Persist the environment + cwd for subsequent calls so that
        // `cd /tmp` in one call leaves the next call rooted at /tmp.
        let new_cwd = executor.cwd().to_string();
        let new_env = executor.into_env();
        *self
            .shell_env
            .lock()
            .map_err(|e| SandboxError::Internal(format!("shell_env mutex poisoned: {e}")))? =
            new_env;
        let cwd_to_store = if workdir.is_some() {
            previous_cwd
        } else {
            new_cwd
        };
        *self
            .shell_cwd
            .lock()
            .map_err(|e| SandboxError::Internal(format!("shell_cwd mutex poisoned: {e}")))? =
            cwd_to_store;

        self.finish_shell_command(command, shell_start, result)
    }

    /// Execute JavaScript code, checking javascript capability and turns budget.
    ///
    /// The JS runtime's `fs.readFileSync`/`fs.writeFileSync` and `simulacra:fs`
    /// `readFile`/`writeFile` route through [`read_file`](Self::read_file) and
    /// [`write_file`](Self::write_file) for capability checking. Remote module
    /// imports route through [`fetch_http`](Self::fetch_http).
    pub fn execute_js(&self, code: &str) -> Result<JsOutput, SandboxError> {
        // Rebuild interest cache to ensure callsite is evaluated
        // against the current thread-local subscriber, not a stale
        // cached decision from a different thread.
        tracing::callsite::rebuild_interest_cache();
        let _span = tracing::info_span!(
            "sandbox_js_exec",
            simulacra.operation.name = "sandbox_js_exec",
        )
        .entered();

        // Check capability
        check_and_journal_capability(
            || self.capability.check_javascript(),
            "execute_js",
            "javascript",
            &self.journal,
            &self.agent_id,
        )?;

        // Atomically reserve the turn before execution.
        reserve_turn(&self.budget, &self.journal, &self.agent_id)?;

        let _script_permit = self
            .script_executor
            .as_ref()
            .map(|executor| {
                executor
                    .try_acquire_permit()
                    .map_err(|e| SandboxError::Internal(format!("script executor error: {e}")))
            })
            .transpose()?;

        let js_start = std::time::Instant::now();

        // Get or create the JS runtime wrapper. The wrapper persists mediated
        // host configuration and remote source caches; each eval uses a fresh
        // QuickJS runtime/context.
        let mut rt_slot = self.js_runtime.lock()?;
        if rt_slot.is_none() {
            // Build an FsProxy that routes through the full Golden Rule chain
            let fs_proxy: Arc<dyn FsProxy> = Arc::new(AgentCellFsProxy {
                vfs: Arc::clone(&self.vfs),
                capability: self.capability.clone(),
                budget: Arc::clone(&self.budget),
                journal: Arc::clone(&self.journal),
                agent_id: self.agent_id.clone(),
            });

            // Build a ModuleFetcher that routes through this AgentCell's fetch_http
            let stubs = self
                .module_stubs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let fetcher: Box<dyn ModuleFetcher> = Box::new(AgentCellModuleFetcher {
                capability: self.capability.clone(),
                budget: Arc::clone(&self.budget),
                journal: Arc::clone(&self.journal),
                agent_id: self.agent_id.clone(),
                http_client: Arc::clone(&self.http_client),
                stubs,
            });

            // Build a FetchProxy that routes through the full Golden Rule chain
            let fetch_proxy: Arc<dyn simulacra_fetch::FetchProxy> = Arc::new(AgentCellFetchProxy {
                capability: self.capability.clone(),
                budget: Arc::clone(&self.budget),
                journal: Arc::clone(&self.journal),
                agent_id: self.agent_id.clone(),
                http_client: Arc::clone(&self.http_client),
                integration_registry: self.integration_registry.clone(),
                tenant_integrations: self.tenant_integrations.clone(),
            });

            match JsRuntime::with_all_options(
                Arc::clone(&self.vfs),
                std::time::Duration::from_secs(5),
                Some(fetcher),
                Some(fs_proxy),
                Some(fetch_proxy),
            ) {
                Ok(rt) => {
                    *rt_slot = Some(rt);
                }
                Err(e) => {
                    // Journal the code execution even on init failure
                    if let Err(err) = self.journal.append(JournalEntry {
                        schema_version: JOURNAL_SCHEMA_VERSION,
                        agent_id: self.agent_id.clone(),
                        timestamp_ms: 0,
                        entry: JournalEntryKind::CodeExecution {
                            language: "javascript".to_string(),
                        },
                    }) {
                        tracing::error!(error = %err, "journal append failed for execute_js");
                    }
                    // Record meters on early-return error path
                    let meters = SandboxMeters::get();
                    let attrs = &[KeyValue::new("simulacra.agent.id", self.agent_id.0.clone())];
                    meters
                        .js_duration
                        .record(js_start.elapsed().as_secs_f64() * 1000.0, attrs);
                    meters.js_requests.add(1, attrs);
                    return Err(SandboxError::Js(e.to_string()));
                }
            }
        }

        // Execute through the wrapper.
        let rt = rt_slot
            .as_ref()
            .ok_or_else(|| SandboxError::Internal("JS runtime not initialized".into()))?;
        let output = rt.eval(code).map_err(|e| {
            // If execution failed due to budget exhaustion (e.g. a module fetch
            // hit the turns limit), surface it as BudgetExhausted so callers get
            // a structured error instead of a generic JS error string.
            let budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
            if budget.max_turns > 0 && budget.used_turns >= budget.max_turns {
                SandboxError::BudgetExhausted(BudgetExhausted {
                    resource: "turns".into(),
                    used: budget.used_turns.to_string(),
                    limit: budget.max_turns.to_string(),
                })
            } else {
                SandboxError::Js(e.to_string())
            }
        });

        // Journal the code execution BEFORE returning (even on failure)
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::CodeExecution {
                language: "javascript".to_string(),
            },
        }) {
            tracing::error!(error = %err, "journal append failed for execute_js");
        }

        // S010: Record OTel meter observations for JS execution
        let meters = SandboxMeters::get();
        let attrs = &[KeyValue::new("simulacra.agent.id", self.agent_id.0.clone())];
        meters
            .js_duration
            .record(js_start.elapsed().as_secs_f64() * 1000.0, attrs);
        meters.js_requests.add(1, attrs);

        output
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

fn ensure_vfs_path_missing(vfs: &Arc<dyn VirtualFs>, path: &str) -> Result<(), SandboxError> {
    if vfs.exists(path) {
        return Err(SandboxError::Vfs(VfsError::AlreadyExists(path.to_string())));
    }
    Ok(())
}

fn ensure_parent_components_are_directories(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
) -> Result<(), SandboxError> {
    for parent in parent_paths(path) {
        match vfs.metadata(&parent) {
            Ok(metadata) if metadata.is_file => {
                return Err(SandboxError::Vfs(VfsError::NotADirectory(parent)));
            }
            Ok(_) | Err(VfsError::NotFound(_)) => {}
            Err(err) => return Err(SandboxError::Vfs(err)),
        }
    }
    Ok(())
}

fn validate_write_precondition(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
    precondition: &VfsWritePrecondition,
) -> Result<(), SandboxError> {
    match precondition {
        VfsWritePrecondition::Any => {
            if let Ok(metadata) = vfs.metadata(path)
                && metadata.is_dir
            {
                return Err(SandboxError::Vfs(VfsError::NotAFile(path.to_string())));
            }
            Ok(())
        }
        VfsWritePrecondition::Missing => ensure_vfs_path_missing(vfs, path),
        VfsWritePrecondition::Matches(expected) => validate_file_matches(vfs, path, expected),
    }
}

fn validate_file_matches(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
    expected: &[u8],
) -> Result<(), SandboxError> {
    let metadata = vfs.metadata(path).map_err(SandboxError::Vfs)?;
    if !metadata.is_file {
        return Err(SandboxError::Vfs(VfsError::NotAFile(path.to_string())));
    }
    let current = vfs.read(path).map_err(SandboxError::Vfs)?;
    if current != expected {
        return Err(SandboxError::Vfs(VfsError::Io(format!(
            "stale write precondition failed for {path}"
        ))));
    }
    Ok(())
}

fn parent_paths(path: &str) -> Vec<String> {
    let Some((parent, _name)) = path.rsplit_once('/') else {
        return Vec::new();
    };
    let parent = if parent.is_empty() { "/" } else { parent };
    let mut paths = Vec::new();
    let mut current = String::new();
    for segment in parent.split('/').filter(|segment| !segment.is_empty()) {
        current.push('/');
        current.push_str(segment);
        paths.push(current.clone());
    }
    paths
}

fn record_missing_parent_dirs(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
    created_parent_dirs: &mut Vec<String>,
) {
    for parent in parent_paths(path) {
        if !vfs.exists(&parent)
            && !created_parent_dirs
                .iter()
                .any(|existing| existing == &parent)
        {
            created_parent_dirs.push(parent);
        }
    }
}

fn upsert_rollback_expectation(
    expectations: &mut Vec<VfsRollbackExpectation>,
    path: &str,
    expected_after: VfsPathState,
) {
    if let Some(existing) = expectations
        .iter_mut()
        .find(|expectation| expectation.path == path)
    {
        existing.expected_after = expected_after;
    } else {
        expectations.push(VfsRollbackExpectation {
            path: path.to_string(),
            expected_after,
        });
    }
}

fn capture_vfs_rollback_entries(
    vfs: &Arc<dyn VirtualFs>,
    expectations: &[VfsRollbackExpectation],
) -> Result<Vec<VfsRollbackEntry>, VfsError> {
    let mut entries = Vec::with_capacity(expectations.len());
    for expectation in expectations {
        let path = &expectation.path;
        let state = match vfs.metadata(path) {
            Ok(metadata) if metadata.is_file => VfsPathState::File(vfs.read(path)?),
            Ok(metadata) if metadata.is_dir => VfsPathState::Dir,
            Ok(_) => VfsPathState::Missing,
            Err(VfsError::NotFound(_)) => VfsPathState::Missing,
            Err(err) => return Err(err),
        };
        entries.push(VfsRollbackEntry {
            path: path.clone(),
            before: state,
            expected_after: expectation.expected_after.clone(),
        });
    }
    Ok(entries)
}

fn vfs_path_matches_state(
    vfs: &Arc<dyn VirtualFs>,
    path: &str,
    expected: &VfsPathState,
) -> Result<bool, VfsError> {
    match expected {
        VfsPathState::Missing => Ok(!vfs.exists(path)),
        VfsPathState::File(expected_data) => match vfs.metadata(path) {
            Ok(metadata) if metadata.is_file => Ok(vfs.read(path)? == *expected_data),
            Ok(_) | Err(VfsError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        },
        VfsPathState::Dir => match vfs.metadata(path) {
            Ok(metadata) => Ok(metadata.is_dir),
            Err(VfsError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        },
    }
}

fn restore_vfs_path_states(
    vfs: &Arc<dyn VirtualFs>,
    states: &[VfsRollbackEntry],
    created_parent_dirs: &[String],
) -> Result<(), VfsError> {
    for entry in states.iter().rev() {
        if !vfs_path_matches_state(vfs, &entry.path, &entry.expected_after)? {
            continue;
        }
        match &entry.before {
            VfsPathState::Missing => {
                if vfs.exists(&entry.path) {
                    vfs.remove(&entry.path)?;
                }
            }
            VfsPathState::File(data) => {
                vfs.write(&entry.path, data)?;
            }
            VfsPathState::Dir => {
                vfs.mkdir(&entry.path)?;
            }
        }
    }

    for path in created_parent_dirs.iter().rev() {
        if matches!(vfs.list_dir(path), Ok(entries) if entries.is_empty()) {
            vfs.remove(path)?;
        }
    }

    Ok(())
}

struct AgentCellShellExternal<'a> {
    cell: &'a AgentCell,
}

impl simulacra_shell::ShellExternalCommand for AgentCellShellExternal<'_> {
    fn run_external(
        &self,
        program: &str,
        args: &[String],
        stdin: &str,
        cwd: &str,
    ) -> Option<simulacra_shell::CommandResult> {
        match program {
            "node" | "nodejs" => Some(self.run_node(args, stdin, cwd)),
            #[cfg(feature = "python")]
            "python" | "python3" => Some(self.run_python(args, stdin, cwd)),
            _ => None,
        }
    }
}

impl AgentCellShellExternal<'_> {
    fn run_node(&self, args: &[String], stdin: &str, cwd: &str) -> simulacra_shell::CommandResult {
        if args.is_empty() {
            return simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: "Usage: node <script.js>\n".into(),
                exit_code: 1,
            };
        }

        if args[0] == "-e" {
            if args.len() < 2 {
                return simulacra_shell::CommandResult {
                    stdout: String::new(),
                    stderr: "node: option -e requires an argument\n".into(),
                    exit_code: 1,
                };
            }
            return self.cell.js_shell_result(&args[1..].join(" "));
        }

        if args[0] == "-" {
            return self.cell.js_shell_result(stdin);
        }

        let script = resolve_shell_path(&args[0], cwd);
        match self.cell.read_file(&script) {
            Ok(bytes) => {
                let code = String::from_utf8_lossy(&bytes);
                self.cell.js_shell_result(&code)
            }
            Err(e) => simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: format!("node: cannot open '{}': {e}\n", args[0]),
                exit_code: 1,
            },
        }
    }

    #[cfg(feature = "python")]
    fn run_python(
        &self,
        args: &[String],
        stdin: &str,
        cwd: &str,
    ) -> simulacra_shell::CommandResult {
        if check_and_journal_capability(
            || self.cell.capability.check_python(),
            "execute_python",
            "python",
            &self.cell.journal,
            &self.cell.agent_id,
        )
        .is_err()
        {
            return simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: "python: capability not granted\n".into(),
                exit_code: 1,
            };
        }

        if args.is_empty() {
            return simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: "Usage: python3 <script.py>\n".into(),
                exit_code: 1,
            };
        }

        if args[0] == "-c" {
            if args.len() < 2 {
                return simulacra_shell::CommandResult {
                    stdout: String::new(),
                    stderr: "python: option -c requires an argument\n".into(),
                    exit_code: 1,
                };
            }
            return self.cell.python_shell_result(&args[1..].join(" "));
        }

        if args[0] == "-" {
            return self.cell.python_shell_result(stdin);
        }

        let script = resolve_shell_path(&args[0], cwd);
        match self.cell.read_file(&script) {
            Ok(bytes) => {
                let code = String::from_utf8_lossy(&bytes);
                self.cell.python_shell_result(&code)
            }
            Err(e) => simulacra_shell::CommandResult {
                stdout: String::new(),
                stderr: format!("python3: cannot open '{}': {e}\n", args[0]),
                exit_code: 1,
            },
        }
    }
}

fn resolve_shell_path(path: &str, cwd: &str) -> String {
    let combined = if path.starts_with('/') {
        path.to_string()
    } else if cwd == "/" {
        format!("/{path}")
    } else {
        format!("{cwd}/{path}")
    };

    let mut parts = Vec::new();
    for part in combined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

#[cfg(feature = "python")]
struct PythonShellDispatcher<'a> {
    cell: &'a AgentCell,
}

#[cfg(feature = "python")]
impl simulacra_python_runtime::ExternalDispatcher for PythonShellDispatcher<'_> {
    fn read_file(&self, path: &str) -> Result<String, String> {
        self.cell
            .read_file(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.cell
            .write_file(path, content.as_bytes())
            .map_err(|e| e.to_string())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.cell.list_dir(path).map_err(|e| e.to_string())
    }

    fn http_get(&self, url: &str) -> Result<String, String> {
        self.cell
            .fetch_http(url, "GET", &[], None, None)
            .map(|response| String::from_utf8_lossy(&response.body).into_owned())
            .map_err(|e| e.to_string())
    }

    fn http_post(&self, url: &str, body: &str) -> Result<String, String> {
        self.cell
            .fetch_http(url, "POST", &[], Some(body.as_bytes()), None)
            .map(|response| String::from_utf8_lossy(&response.body).into_owned())
            .map_err(|e| e.to_string())
    }

    fn env_get(&self, _name: &str) -> Result<Option<String>, String> {
        Ok(None)
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

/// Core read_file logic following the Golden Rule: span -> capability -> budget -> execute -> journal -> return.
///
/// Shared by [`AgentCell::read_file`] and [`AgentCellFsProxy::read_file`](fs_proxy::AgentCellFsProxy).
fn read_file_inner(
    path: &str,
    vfs: &Arc<dyn VirtualFs>,
    capability: &CapabilityToken,
    budget: &Arc<Mutex<ResourceBudget>>,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
) -> Result<Vec<u8>, SandboxError> {
    let _span = tracing::info_span!(
        "sandbox_read_file",
        simulacra.operation.name = "sandbox_read_file",
        simulacra.vfs.path = path,
    )
    .entered();

    // Check capability. Memory paths are gated by MemoryCapability, not the
    // generic paths_read glob — attribute the denial counter accordingly so
    // operators can filter memory denials separately from generic-glob denials.
    check_and_journal_capability(
        || capability.check_path_read(path),
        "read_file",
        cap_name_for_read(path),
        journal,
        agent_id,
    )?;

    // Check global budget
    {
        let b = budget
            .lock()
            .map_err(|e| SandboxError::Internal(format!("budget mutex poisoned: {e}")))?;
        if let Err(exhausted) = b.check_budget() {
            journal_budget_exhaustion(journal, agent_id, &exhausted);
            tracing::warn!(
                simulacra.budget.resource = %exhausted.resource,
                simulacra.budget.used = %exhausted.used,
                simulacra.budget.limit = %exhausted.limit,
                "budget exhausted"
            );
            return Err(SandboxError::BudgetExhausted(exhausted));
        }
    }

    // Execute
    let data = match vfs.read(path) {
        Ok(data) => data,
        Err(err) => {
            if let Err(journal_err) = journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::ToolResult {
                    tool_call_id: None,
                    tool_name: "read_file".to_string(),
                    content: err.to_string(),
                    is_error: true,
                },
            }) {
                tracing::error!(error = %journal_err, "journal append failed for read_file error");
            }
            return Err(SandboxError::Vfs(err));
        }
    };

    // Journal the read
    if let Err(err) = journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::ToolResult {
            tool_call_id: None,
            tool_name: "read_file".to_string(),
            content: format!("read {} bytes from {}", data.len(), path),
            is_error: false,
        },
    }) {
        tracing::error!(error = %err, "journal append failed for read_file");
    }

    Ok(data)
}

/// Core write_file logic following the Golden Rule: span -> capability -> budget -> journal -> execute -> budget increment.
///
/// Shared by [`AgentCell::write_file`] and [`AgentCellFsProxy::write_file`](fs_proxy::AgentCellFsProxy).
fn write_file_inner(
    path: &str,
    data: &[u8],
    vfs: &Arc<dyn VirtualFs>,
    capability: &CapabilityToken,
    budget: &Arc<Mutex<ResourceBudget>>,
    journal: &Arc<dyn JournalStorage>,
    agent_id: &AgentId,
) -> Result<(), SandboxError> {
    let _span = tracing::info_span!(
        "sandbox_write_file",
        simulacra.operation.name = "sandbox_write_file",
        simulacra.vfs.path = path,
        simulacra.vfs.bytes = data.len() as u64,
    )
    .entered();

    // Check capability. Memory paths are gated by MemoryCapability, not the
    // generic paths_write glob — attribute the denial counter accordingly so
    // operators can filter memory denials separately from generic-glob denials.
    check_and_journal_capability(
        || capability.check_path_write(path),
        "write_file",
        cap_name_for_write(path),
        journal,
        agent_id,
    )?;

    let write_bytes = data.len() as u64;
    reserve_vfs_bytes(budget, write_bytes, journal, agent_id)?;

    // Journal the write (before execution)
    if let Err(err) = journal.append(JournalEntry {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_id: agent_id.clone(),
        timestamp_ms: 0,
        entry: JournalEntryKind::FileWrite {
            path: path.to_string(),
            size_bytes: data.len() as u64,
        },
    }) {
        tracing::error!(error = %err, "journal append failed for write_file");
    }

    // Execute. If the VFS rejects the write, roll back the byte reservation.
    if let Err(err) = vfs.write(path, data) {
        release_vfs_bytes(budget, write_bytes)?;
        return Err(SandboxError::Vfs(err));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulacra_http::{HttpError, HttpRequest, HttpResponse as FhHttpResponse};
    use simulacra_types::{
        CheckpointData, JournalError, NetworkPermission, PathPattern, TokenUsage,
    };
    use simulacra_vfs::MemoryFs;

    // ── W2: telemetry attribution for memory denials ─────────────────────

    #[test]
    fn cap_name_for_read_routes_memory_paths_to_memory_search_scopes() {
        // Memory paths must be labeled as memory denials so the OTel counter
        // `simulacra.sandbox.capability.denials{operation="memory_search_scopes"}`
        // attributes correctly. Without this, memory denials would land under
        // `paths_read` and mask the real cause.
        assert_eq!(
            cap_name_for_read("/var/memory/self/note.md"),
            "memory_search_scopes"
        );
        assert_eq!(
            cap_name_for_read("/mnt/policies/hr.pdf"),
            "memory_search_scopes"
        );
        assert_eq!(cap_name_for_read("/var/memory"), "memory_search_scopes");
        assert_eq!(cap_name_for_read("/mnt"), "memory_search_scopes");
    }

    #[test]
    fn cap_name_for_read_routes_non_memory_paths_to_paths_read() {
        assert_eq!(cap_name_for_read("/workspace/file.md"), "paths_read");
        assert_eq!(cap_name_for_read("/etc/passwd"), "paths_read");
        // Lookalikes must NOT be classified as memory.
        assert_eq!(cap_name_for_read("/var/memory.bak/x"), "paths_read");
        assert_eq!(cap_name_for_read("/mntfoo/x"), "paths_read");
        assert_eq!(cap_name_for_read("/Var/Memory/x"), "paths_read");
    }

    #[test]
    fn cap_name_for_write_routes_memory_paths_to_memory_write_scopes() {
        assert_eq!(
            cap_name_for_write("/var/memory/self/note.md"),
            "memory_write_scopes"
        );
        assert_eq!(
            cap_name_for_write("/mnt/policies/hr.pdf"),
            "memory_write_scopes"
        );
        assert_eq!(cap_name_for_write("/var/memory"), "memory_write_scopes");
    }

    #[test]
    fn cap_name_for_write_routes_non_memory_paths_to_paths_write() {
        assert_eq!(cap_name_for_write("/workspace/file.md"), "paths_write");
        assert_eq!(cap_name_for_write("/var/memory.bak/x"), "paths_write");
        assert_eq!(cap_name_for_write("/mntfoo/x"), "paths_write");
    }

    struct NullJournal;

    #[derive(Default)]
    struct CapturingJournal {
        entries: Mutex<Vec<JournalEntry>>,
    }

    impl CapturingJournal {
        fn entries(&self) -> Vec<JournalEntry> {
            self.entries.lock().unwrap().clone()
        }
    }

    impl JournalStorage for NullJournal {
        fn append(&self, _entry: JournalEntry) -> Result<(), JournalError> {
            Ok(())
        }
        fn read_all(&self, _agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(vec![])
        }
        fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
            Ok(TokenUsage::default())
        }
        fn save_checkpoint(
            &self,
            _agent_id: &AgentId,
            _after_entry: usize,
            _data: CheckpointData,
        ) -> Result<(), JournalError> {
            Ok(())
        }
        fn fork_from(
            &self,
            _agent_id: &AgentId,
            _checkpoint_idx: usize,
        ) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(vec![])
        }
        fn read_from(
            &self,
            _agent_id: &AgentId,
            _start_index: usize,
        ) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(vec![])
        }
    }

    impl JournalStorage for CapturingJournal {
        fn append(&self, entry: JournalEntry) -> Result<(), JournalError> {
            self.entries.lock().unwrap().push(entry);
            Ok(())
        }

        fn read_all(&self, _agent_id: &AgentId) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(self.entries())
        }

        fn query_token_usage(&self, _agent_id: &AgentId) -> Result<TokenUsage, JournalError> {
            Ok(TokenUsage::default())
        }

        fn save_checkpoint(
            &self,
            _agent_id: &AgentId,
            _after_entry: usize,
            _data: CheckpointData,
        ) -> Result<(), JournalError> {
            Ok(())
        }

        fn fork_from(
            &self,
            _agent_id: &AgentId,
            _checkpoint_idx: usize,
        ) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(vec![])
        }

        fn read_from(
            &self,
            _agent_id: &AgentId,
            _start_index: usize,
        ) -> Result<Vec<JournalEntry>, JournalError> {
            Ok(vec![])
        }
    }

    fn make_cell(capability: CapabilityToken) -> AgentCell {
        let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let budget = Arc::new(Mutex::new(ResourceBudget::new(
            0,
            0,
            rust_decimal::Decimal::ZERO,
            0,
        )));
        let journal: Arc<dyn JournalStorage> = Arc::new(NullJournal);
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        AgentCell::new(vfs, capability, budget, journal, http_client)
    }

    fn make_cell_with_journal(
        capability: CapabilityToken,
        journal: Arc<dyn JournalStorage>,
    ) -> AgentCell {
        let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let budget = Arc::new(Mutex::new(ResourceBudget::new(
            0,
            0,
            rust_decimal::Decimal::ZERO,
            0,
        )));
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(simulacra_http::UreqHttpClient::default());
        AgentCell::new(vfs, capability, budget, journal, http_client)
    }

    #[test]
    fn shell_denied_without_capability() {
        let cell = make_cell(CapabilityToken {
            shell: false,
            ..Default::default()
        });
        let err = cell.execute_shell("echo hello").unwrap_err();
        assert!(matches!(err, SandboxError::CapabilityDenied(_)));
    }

    #[test]
    fn shell_allowed_with_capability() {
        let cell = make_cell(CapabilityToken {
            shell: true,
            ..Default::default()
        });
        let result = cell.execute_shell("echo hello").unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "hello");
    }

    #[test]
    fn js_denied_without_capability() {
        let cell = make_cell(CapabilityToken {
            javascript: false,
            ..Default::default()
        });
        let err = cell.execute_js("1+1").unwrap_err();
        assert!(matches!(err, SandboxError::CapabilityDenied(_)));
    }

    #[test]
    fn shell_denial_surfaces_operation_and_reason_to_agent() {
        let cell = make_cell(CapabilityToken {
            shell: false,
            ..Default::default()
        });

        let err = cell.execute_shell("echo hello").unwrap_err();
        let SandboxError::CapabilityDenied(denied) = err else {
            panic!("expected a capability denial");
        };

        assert_eq!(denied.operation, "shell");
        assert_eq!(denied.reason, "shell capability not granted");
    }

    #[test]
    fn shell_execution_records_shell_command_entry_before_return() {
        let journal = Arc::new(CapturingJournal::default());
        let cell = make_cell_with_journal(
            CapabilityToken {
                shell: true,
                ..Default::default()
            },
            journal.clone(),
        );

        let result = cell.execute_shell("echo hello").unwrap();
        let entries = journal.entries();

        assert_eq!(result.exit_code, 0);
        assert!(entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "echo hello" && *exit_code == 0
            )
        }));
    }

    #[test]
    fn file_write_records_file_write_entry_before_return() {
        let journal = Arc::new(CapturingJournal::default());
        let cell = make_cell_with_journal(
            CapabilityToken {
                paths_write: vec![PathPattern("/**".into())],
                ..Default::default()
            },
            journal.clone(),
        );

        cell.write_file("/tmp/output.txt", b"hello journal")
            .unwrap();

        let entries = journal.entries();
        assert!(entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::FileWrite { path, size_bytes }
                    if path == "/tmp/output.txt" && *size_bytes == 13
            )
        }));
    }

    #[test]
    fn js_execution_failure_still_records_code_execution_entry_before_return() {
        let journal = Arc::new(CapturingJournal::default());
        let cell = make_cell_with_journal(
            CapabilityToken {
                javascript: true,
                ..Default::default()
            },
            journal.clone(),
        );

        let err = cell
            .execute_js("function broken(")
            .expect_err("invalid JavaScript should fail");
        assert!(matches!(err, SandboxError::Js(_)));

        let entries = journal.entries();
        assert!(entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::CodeExecution { language } if language == "javascript"
            )
        }));
    }

    #[test]
    fn http_failure_still_records_http_request_entry_before_return() {
        let journal = Arc::new(CapturingJournal::default());
        let cell = make_cell_with_journal(
            CapabilityToken {
                network: vec![NetworkPermission("net:127.0.0.1".into())],
                ..Default::default()
            },
            journal.clone(),
        );

        let err = cell
            .fetch_http("http://127.0.0.1:9/journal-red", "GET", &[], None, None)
            .expect_err("connection-refused HTTP request should fail");
        assert!(matches!(err, SandboxError::Http(_)));

        let entries = journal.entries();
        assert!(entries.iter().any(|entry| {
            matches!(
                &entry.entry,
                JournalEntryKind::HttpRequest { method, url, status }
                    if method == "GET"
                        && url == "http://127.0.0.1:9/journal-red"
                        && *status == 0
            )
        }));
    }

    // ── Mock HttpClient for shell HTTP proxy tests ─────────────────────

    /// A mock [`HttpClient`] that returns a canned response or error.
    struct MockHttpClient {
        response: Mutex<Option<Result<FhHttpResponse, HttpError>>>,
    }

    impl MockHttpClient {
        fn with_ok(status: u16, body: &[u8]) -> Self {
            Self {
                response: Mutex::new(Some(Ok(FhHttpResponse {
                    status,
                    status_text: "OK".into(),
                    headers: vec![],
                    body: body.to_vec(),
                    url: String::new(),
                    redirected: false,
                }))),
            }
        }
    }

    impl simulacra_http::HttpClient for MockHttpClient {
        fn execute(&self, _request: &HttpRequest) -> Result<FhHttpResponse, HttpError> {
            let slot = self.response.lock().unwrap();
            // Re-create the response each time so the mock is reusable
            match slot.as_ref() {
                Some(Ok(resp)) => Ok(resp.clone()),
                Some(Err(e)) => Err(HttpError::Network(e.to_string())),
                None => panic!("MockHttpClient: no response configured"),
            }
        }
    }

    fn make_cell_full(
        capability: CapabilityToken,
        budget: Arc<Mutex<ResourceBudget>>,
        journal: Arc<dyn JournalStorage>,
        http_client: Arc<dyn simulacra_http::HttpClient>,
    ) -> AgentCell {
        let vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        AgentCell::new(vfs, capability, budget, journal, http_client)
    }

    // ── Task 5: shell curl routes through AgentCellShellHttpProxy ─────

    #[test]
    fn shell_curl_denied_when_network_capability_missing() {
        let journal = Arc::new(CapturingJournal::default());
        let budget = Arc::new(Mutex::new(ResourceBudget::new(
            0,
            0,
            rust_decimal::Decimal::ZERO,
            0,
        )));
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(MockHttpClient::with_ok(200, b"should not reach"));
        let cell = make_cell_full(
            CapabilityToken {
                shell: true,
                network: vec![], // no network permission
                ..Default::default()
            },
            budget,
            journal,
            http_client,
        );

        let result = cell
            .execute_shell("curl http://denied.example.com")
            .unwrap();
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("capability denied"),
            "stderr should contain 'capability denied', got: {}",
            result.stderr
        );
    }

    // ── Task 6: Budget verification ─────────────────────────────────────

    #[test]
    fn shell_curl_increments_used_turns_for_both_shell_and_http() {
        let journal = Arc::new(CapturingJournal::default());
        let budget = Arc::new(Mutex::new(ResourceBudget::new(
            10,
            0,
            rust_decimal::Decimal::ZERO,
            0,
        )));
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(MockHttpClient::with_ok(200, b"hello"));
        let cell = make_cell_full(
            CapabilityToken {
                shell: true,
                network: vec![NetworkPermission("net:allowed.example.com".into())],
                ..Default::default()
            },
            Arc::clone(&budget),
            journal,
            http_client,
        );

        let result = cell
            .execute_shell("curl http://allowed.example.com/data")
            .unwrap();
        assert_eq!(result.exit_code, 0);

        let b = budget.lock().unwrap();
        // 1 turn for the shell command + 1 turn for the HTTP request = 2
        assert_eq!(
            b.used_turns, 2,
            "expected 2 used_turns (1 shell + 1 HTTP), got {}",
            b.used_turns
        );
    }

    #[test]
    fn shell_curl_records_both_shell_and_http_journal_entries() {
        let journal = Arc::new(CapturingJournal::default());
        let budget = Arc::new(Mutex::new(ResourceBudget::new(
            10,
            0,
            rust_decimal::Decimal::ZERO,
            0,
        )));
        let http_client: Arc<dyn simulacra_http::HttpClient> =
            Arc::new(MockHttpClient::with_ok(200, b"response body"));
        let cell = make_cell_full(
            CapabilityToken {
                shell: true,
                network: vec![NetworkPermission("net:api.example.com".into())],
                ..Default::default()
            },
            budget,
            journal.clone(),
            http_client,
        );

        let result = cell
            .execute_shell("curl http://api.example.com/endpoint")
            .unwrap();
        assert_eq!(result.exit_code, 0);

        let entries = journal.entries();

        // Verify an HttpRequest entry was journaled
        let has_http_entry = entries.iter().any(|e| {
            matches!(
                &e.entry,
                JournalEntryKind::HttpRequest { method, url, status }
                    if method == "GET"
                        && url == "http://api.example.com/endpoint"
                        && *status == 200
            )
        });
        assert!(
            has_http_entry,
            "journal should contain an HttpRequest entry for the curl call"
        );

        // Verify a ShellCommand entry was journaled
        let has_shell_entry = entries.iter().any(|e| {
            matches!(
                &e.entry,
                JournalEntryKind::ShellCommand { command, exit_code }
                    if command == "curl http://api.example.com/endpoint" && *exit_code == 0
            )
        });
        assert!(
            has_shell_entry,
            "journal should contain a ShellCommand entry for the curl call"
        );
    }
}
