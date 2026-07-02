//! Agent virtual process filesystem (`/proc`).
//!
//! [`ProcFs`] is a [`VirtualFs`] wrapper that intercepts reads to `/proc/**`
//! and returns live runtime state as virtual files. All other paths and all
//! `/proc/mailbox/**` paths are delegated to the inner VFS unchanged.
//!
//! # Design
//!
//! - All `/proc/**` reads (except `/proc/mailbox/**`) are computed at read
//!   time from [`ProcState`] — no caching.
//! - All writes/removes/mkdir on `/proc/**` (except `/proc/mailbox/`) return
//!   [`VfsError::PermissionDenied`].
//! - `/proc/mailbox/**` reads and writes are delegated to the inner VFS.
//!
//! # Traits
//!
//! To avoid circular crate dependencies, tool and hook access is abstracted
//! via narrow traits ([`ToolLister`] and [`HookLister`]) that callers must
//! implement on their concrete types before wiring in a [`ProcFs`].

use std::sync::atomic::Ordering;

use simulacra_types::{FsMetadata, VfsError, VfsSnapshot, VirtualFs};
use tracing::{debug, info_span, warn};

use crate::path::normalize;

mod state;

pub use state::{HookLister, ProcFs, ProcState, ToolLister};

// ---------------------------------------------------------------------------
// Path classification helpers
// ---------------------------------------------------------------------------

/// Returns `true` if `path` is under `/proc/` (or exactly `/proc`).
fn is_proc(path: &str) -> bool {
    path == "/proc" || path.starts_with("/proc/")
}

/// Returns `true` if `path` is under `/proc/mailbox/` (or exactly
/// `/proc/mailbox`). These paths are always delegated to the inner VFS.
fn is_mailbox(path: &str) -> bool {
    path == "/proc/mailbox" || path.starts_with("/proc/mailbox/")
}

/// Strip the `/proc/` prefix and return the remainder, with any trailing slash removed.
/// Returns `None` if the path is exactly `/proc` or `/proc/`.
fn proc_tail(path: &str) -> Option<&str> {
    let normalized = path.trim_end_matches('/');
    if normalized == "/proc" {
        None
    } else {
        normalized.strip_prefix("/proc/")
    }
}

// ---------------------------------------------------------------------------
// Value computation
// ---------------------------------------------------------------------------

fn budget_value(state: &ProcState, name: &str) -> Option<String> {
    let b = state.budget.lock().unwrap();
    match name {
        "max_tokens" => Some(b.max_tokens.to_string()),
        "used_tokens" => Some(b.used_tokens.to_string()),
        "remaining_tokens" => {
            if b.max_tokens == 0 {
                Some("0".to_string())
            } else {
                Some(b.max_tokens.saturating_sub(b.used_tokens).to_string())
            }
        }
        "max_turns" => Some(b.max_turns.to_string()),
        "used_turns" => Some(b.used_turns.to_string()),
        "remaining_turns" => {
            if b.max_turns == 0 {
                Some("0".to_string())
            } else {
                Some(
                    (b.max_turns as u64)
                        .saturating_sub(b.used_turns as u64)
                        .to_string(),
                )
            }
        }
        "max_fuel" => Some(b.max_fuel.to_string()),
        "used_fuel" => Some(b.used_fuel.to_string()),
        "max_cost" => Some(format!("{:.2}", b.max_cost)),
        "used_cost" => Some(format!("{:.2}", b.used_cost)),
        _ => None,
    }
}

fn capabilities_value(state: &ProcState, name: &str) -> Option<String> {
    let c = &state.capabilities;
    match name {
        "shell" => Some(if c.shell { "true" } else { "false" }.to_string()),
        "javascript" => Some(if c.javascript { "true" } else { "false" }.to_string()),
        "python" => Some(if c.python { "true" } else { "false" }.to_string()),
        "network" => Some(
            c.network
                .iter()
                .map(|n| n.0.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "mcp_tools" => Some(c.mcp_tools.join("\n")),
        "paths_read" => Some(
            c.paths_read
                .iter()
                .map(|p| p.0.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "paths_write" => Some(
            c.paths_write
                .iter()
                .map(|p| p.0.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn session_value(state: &ProcState, name: &str) -> Option<String> {
    match name {
        "id" => Some(state.session_id.clone()),
        "uptime_ms" => Some(state.session_start.elapsed().as_millis().to_string()),
        "journal_entries" => Some(state.journal_entries.load(Ordering::Relaxed).to_string()),
        _ => None,
    }
}

fn agent_value(state: &ProcState, name: &str) -> Option<String> {
    match name {
        "id" => Some(state.agent_id.clone()),
        "name" => Some(state.agent_name.clone()),
        "model" => Some(state.model.clone()),
        "turn" => Some(state.turn.load(Ordering::Relaxed).to_string()),
        "parent_id" => Some(state.parent_id.as_deref().unwrap_or("").to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Directory listings
// ---------------------------------------------------------------------------

fn proc_list_dir(state: &ProcState, path: &str) -> Result<Vec<String>, VfsError> {
    let tail = match proc_tail(path) {
        None => {
            // "/proc" itself
            return Ok(vec![
                "agent".to_string(),
                "budget".to_string(),
                "capabilities".to_string(),
                "hooks".to_string(),
                "mailbox".to_string(),
                "session".to_string(),
                "tools".to_string(),
            ]);
        }
        Some(t) => t,
    };

    match tail {
        "agent" => Ok(vec![
            "id".to_string(),
            "model".to_string(),
            "name".to_string(),
            "parent_id".to_string(),
            "turn".to_string(),
        ]),
        "budget" => Ok(vec![
            "max_cost".to_string(),
            "max_fuel".to_string(),
            "max_tokens".to_string(),
            "max_turns".to_string(),
            "remaining_tokens".to_string(),
            "remaining_turns".to_string(),
            "used_cost".to_string(),
            "used_fuel".to_string(),
            "used_tokens".to_string(),
            "used_turns".to_string(),
        ]),
        "capabilities" => Ok(vec![
            "javascript".to_string(),
            "mcp_tools".to_string(),
            "network".to_string(),
            "paths_read".to_string(),
            "paths_write".to_string(),
            "python".to_string(),
            "shell".to_string(),
        ]),
        "tools" => {
            let mut names = state.tools.tool_names();
            names.sort();
            Ok(names)
        }
        "session" => Ok(vec![
            "id".to_string(),
            "journal_entries".to_string(),
            "uptime_ms".to_string(),
        ]),
        "hooks" => Ok(vec![
            "http_request".to_string(),
            "llm".to_string(),
            "spawn".to_string(),
            "tool_call".to_string(),
        ]),
        _ => Err(VfsError::NotFound(path.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Read dispatch
// ---------------------------------------------------------------------------

fn proc_read(state: &ProcState, path: &str) -> Result<Vec<u8>, VfsError> {
    let tail = proc_tail(path).ok_or_else(|| VfsError::NotAFile(path.to_string()))?;

    let parts: Vec<&str> = tail.splitn(2, '/').collect();
    let value: Option<String> = match parts[0] {
        "agent" => {
            if parts.len() < 2 {
                return Err(VfsError::NotAFile(path.to_string()));
            }
            agent_value(state, parts[1])
        }
        "budget" => {
            if parts.len() < 2 {
                return Err(VfsError::NotAFile(path.to_string()));
            }
            budget_value(state, parts[1])
        }
        "capabilities" => {
            if parts.len() < 2 {
                return Err(VfsError::NotAFile(path.to_string()));
            }
            capabilities_value(state, parts[1])
        }
        "tools" => {
            if parts.len() < 2 {
                return Err(VfsError::NotAFile(path.to_string()));
            }
            state.tools.tool_json(parts[1])
        }
        "session" => {
            if parts.len() < 2 {
                return Err(VfsError::NotAFile(path.to_string()));
            }
            session_value(state, parts[1])
        }
        "hooks" => {
            if parts.len() < 2 {
                return Err(VfsError::NotAFile(path.to_string()));
            }
            // All four known operations return a valid (possibly empty) string.
            match parts[1] {
                "tool_call" | "llm" | "spawn" | "http_request" => {
                    Some(state.hooks.hook_names(parts[1]).join("\n"))
                }
                _ => None,
            }
        }
        _ => None,
    };

    match value {
        Some(v) => Ok(v.into_bytes()),
        None => Err(VfsError::NotFound(path.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Metadata / exists helpers
// ---------------------------------------------------------------------------

fn proc_metadata(state: &ProcState, path: &str) -> Result<FsMetadata, VfsError> {
    let normalized = path.trim_end_matches('/');
    if normalized == "/proc" {
        return Ok(FsMetadata {
            is_file: false,
            is_dir: true,
            size: 0,
        });
    }

    let tail = proc_tail(path).unwrap_or("");

    // Known top-level directories
    match tail {
        "agent" | "budget" | "capabilities" | "tools" | "session" | "hooks" | "mailbox" => {
            return Ok(FsMetadata {
                is_file: false,
                is_dir: true,
                size: 0,
            });
        }
        _ => {}
    }

    // Try to read as a file to get its byte length
    match proc_read(state, path) {
        Ok(bytes) => Ok(FsMetadata {
            is_file: true,
            is_dir: false,
            size: bytes.len() as u64,
        }),
        Err(VfsError::NotFound(_)) => Err(VfsError::NotFound(path.to_string())),
        Err(e) => Err(e),
    }
}

fn proc_exists(state: &ProcState, path: &str) -> bool {
    proc_metadata(state, path).is_ok()
}

// ---------------------------------------------------------------------------
// VirtualFs impl
// ---------------------------------------------------------------------------

impl<V: VirtualFs> VirtualFs for ProcFs<V> {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let path = normalize(path);
        let path = path.as_str();

        if is_mailbox(path) {
            return self.inner.read(path);
        }

        if is_proc(path) {
            let category = proc_tail(path)
                .and_then(|t| t.split('/').next())
                .unwrap_or("proc");

            let _span = info_span!(
                "simulacra_procfs_read",
                "simulacra.procfs.path" = path,
                "simulacra.procfs.category" = category,
            )
            .entered();

            let result = proc_read(&self.state, path);
            match &result {
                Ok(bytes) => {
                    debug!(
                        simulacra.procfs.path = path,
                        simulacra.procfs.value_len = bytes.len(),
                        "procfs read"
                    );
                    // Emit counter event for simulacra.procfs.reads metric with category label.
                    tracing::event!(
                        tracing::Level::DEBUG,
                        "simulacra.procfs.reads" = 1u64,
                        category = category,
                        "simulacra_procfs_reads_counter"
                    );
                }
                Err(e) => {
                    debug!(simulacra.procfs.path = path, error = %e, "procfs read error");
                }
            }
            return result;
        }

        self.inner.read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let path = normalize(path);
        let path = path.as_str();

        if is_mailbox(path) {
            return self.inner.write(path, data);
        }

        if is_proc(path) {
            warn!(
                simulacra.procfs.path = path,
                "write attempt to read-only procfs path"
            );
            return Err(VfsError::PermissionDenied(format!("{path} is read-only")));
        }

        self.inner.write(path, data)
    }

    fn exists(&self, path: &str) -> bool {
        let path = normalize(path);
        let path = path.as_str();

        if is_mailbox(path) {
            return self.inner.exists(path);
        }

        if is_proc(path) {
            return proc_exists(&self.state, path);
        }

        self.inner.exists(path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let path = normalize(path);
        let path = path.as_str();

        if is_mailbox(path) {
            // Delegate to inner VFS; if mailbox dir has not been created yet, return empty list.
            return match self.inner.list_dir(path) {
                Ok(entries) => Ok(entries),
                Err(VfsError::NotFound(_)) => Ok(vec![]),
                Err(e) => Err(e),
            };
        }

        if is_proc(path) {
            let _span =
                info_span!("simulacra_procfs_list_dir", "simulacra.procfs.path" = path).entered();
            return proc_list_dir(&self.state, path);
        }

        self.inner.list_dir(path)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        let path = path.as_str();

        if is_mailbox(path) {
            return self.inner.mkdir(path);
        }

        if is_proc(path) {
            warn!(
                simulacra.procfs.path = path,
                "mkdir attempt to read-only procfs path"
            );
            return Err(VfsError::PermissionDenied(format!("{path} is read-only")));
        }

        self.inner.mkdir(path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        let path = path.as_str();

        if is_mailbox(path) {
            return self.inner.remove(path);
        }

        if is_proc(path) {
            warn!(
                simulacra.procfs.path = path,
                "remove attempt to read-only procfs path"
            );
            return Err(VfsError::PermissionDenied(format!("{path} is read-only")));
        }

        self.inner.remove(path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let path = normalize(path);
        let path = path.as_str();

        if is_mailbox(path) {
            return self.inner.metadata(path);
        }

        if is_proc(path) {
            return proc_metadata(&self.state, path);
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
