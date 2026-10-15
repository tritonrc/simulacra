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
//! - The atomic calls (`write_if`, `rename`, `apply_batch`) follow the same
//!   split: any touched path under the read-only part of `/proc` is refused
//!   before anything is applied, the rest is forwarded, and `/proc/mailbox`
//!   paths reach the inner VFS, which answers for them.
//!
//! # Traits
//!
//! To avoid circular crate dependencies, tool and hook access is abstracted
//! via narrow traits ([`ToolLister`] and [`HookLister`]) that callers must
//! implement on their concrete types before wiring in a [`ProcFs`].

use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot, VfsWatcher, VirtualFs,
};
use tracing::{debug, info_span, warn};

use crate::atomic_paths::{self, normalize_mutations};
use crate::path::normalize;

mod atomic;
mod state;
mod values;

use values::{agent_value, budget_value, capabilities_value, session_value};

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
        return Ok(FsMetadata::dir());
    }

    let tail = proc_tail(path).unwrap_or("");

    // Known top-level directories
    match tail {
        "agent" | "budget" | "capabilities" | "tools" | "session" | "hooks" | "mailbox" => {
            return Ok(FsMetadata::dir());
        }
        _ => {}
    }

    // Try to read as a file to get its byte length
    match proc_read(state, path) {
        Ok(bytes) => Ok(FsMetadata::file(bytes.len() as u64)),
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

        // Refuses `/proc` and everything under it except `/proc/mailbox`,
        // and a whole-tree removal from `/`, which this layer refuses rather
        // than leave to the inner filesystem.
        self.gate(&atomic_paths::removed(path))?;
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

    fn subscribe(&self, prefix: &str) -> VfsWatcher {
        self.inner.subscribe(prefix)
    }

    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: Precondition,
    ) -> Result<(), VfsError> {
        self.gate(&atomic_paths::written(path))?;
        self.inner.write_if(&normalize(path), data, precondition)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.gate(&atomic_paths::renamed(from, to))?;
        self.inner.rename(&normalize(from), &normalize(to))
    }

    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        self.gate(&atomic_paths::batched(mutations))?;
        self.inner.apply_batch(&normalize_mutations(mutations))
    }

    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        let normalized = normalize(path);
        if is_proc(&normalized) && !is_mailbox(&normalized) {
            // ProcFs computes these listings itself; the inner bound is not theirs.
            return Ok(DirListing::complete(self.list_dir(path)?));
        }
        match self.inner.list_dir_bounded(&normalized) {
            // Same as `list_dir`: a mailbox directory not yet created is empty.
            Err(VfsError::NotFound(_)) if is_mailbox(&normalized) => Ok(DirListing::default()),
            other => other,
        }
    }
}
