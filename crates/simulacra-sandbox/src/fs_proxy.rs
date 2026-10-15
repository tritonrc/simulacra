//! FsProxy implementation that routes through the full Golden Rule chain.
//!
//! Delegates to the same [`read_file_inner`](super::read_file_inner) and
//! [`write_file_inner`](super::write_file_inner) functions used by
//! [`AgentCell::read_file`](super::AgentCell::read_file) and
//! [`AgentCell::write_file`](super::AgentCell::write_file), ensuring that
//! JS host functions (`fs.readFileSync`, `fs.writeFileSync`, `simulacra:fs`) get
//! capability checks, applicable budget checks, journal entries, OTel spans, and
//! budget counter increments.

use simulacra_quickjs::FsProxy;
use simulacra_types::{
    AgentId, CapabilityToken, FsMetadata, JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind,
    JournalStorage, ResourceBudget, VfsError, VirtualFs,
};
use std::sync::{Arc, Mutex};

use crate::file_io::{read_file_inner, write_file_inner};
use crate::fs_proxy_append::append_conditionally;
use crate::fs_proxy_rename::rename_replacing;
use crate::guards::{check_and_journal_capability, release_vfs_bytes, reserve_vfs_bytes};
use crate::{cap_name_for_read, cap_name_for_write};

pub(crate) struct AgentCellFsProxy {
    pub(crate) vfs: Arc<dyn VirtualFs>,
    pub(crate) capability: CapabilityToken,
    pub(crate) budget: Arc<Mutex<ResourceBudget>>,
    pub(crate) journal: Arc<dyn JournalStorage>,
    pub(crate) agent_id: AgentId,
}

impl AgentCellFsProxy {
    pub(crate) fn check_read(&self, path: &str, operation: &str) -> Result<(), String> {
        check_and_journal_capability(
            || self.capability.check_path_read(path),
            operation,
            cap_name_for_read(path),
            &self.journal,
            &self.agent_id,
        )
        .map_err(|e| e.to_string())
    }

    pub(crate) fn check_write(&self, path: &str, operation: &str) -> Result<(), String> {
        check_and_journal_capability(
            || self.capability.check_path_write(path),
            operation,
            cap_name_for_write(path),
            &self.journal,
            &self.agent_id,
        )
        .map_err(|e| e.to_string())
    }

    pub(crate) fn reserve_write_budget(&self, bytes: usize) -> Result<(), String> {
        reserve_vfs_bytes(&self.budget, bytes as u64, &self.journal, &self.agent_id)
            .map_err(|e| e.to_string())
    }

    pub(crate) fn release_write_budget(&self, bytes: usize) -> Result<(), String> {
        release_vfs_bytes(&self.budget, bytes as u64).map_err(|e| e.to_string())
    }

    pub(crate) fn journal_file_write(&self, path: &str, size_bytes: usize) {
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::FileWrite {
                path: path.to_string(),
                size_bytes: size_bytes as u64,
            },
        }) {
            // The path is caller-controlled; only the error and the operation
            // kind are logged, never the script-chosen path.
            tracing::error!(error = %err, "journal append failed for fs proxy file write");
        }
    }

    pub(crate) fn journal_result(&self, operation: &str, content: String, is_error: bool) {
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::ToolResult {
                tool_call_id: None,

                tool_name: operation.to_string(),
                content,
                is_error,
                provider_content: Vec::new(),
            },
        }) {
            tracing::error!(error = %err, operation, "journal append failed for fs proxy");
        }
    }

    pub(crate) fn finish<T>(
        &self,
        operation: &str,
        result: Result<T, simulacra_types::VfsError>,
        ok_content: impl FnOnce(&T) -> String,
    ) -> Result<T, String> {
        match result {
            Ok(value) => {
                self.journal_result(operation, ok_content(&value), false);
                Ok(value)
            }
            Err(err) => {
                self.journal_result(operation, err.to_string(), true);
                Err(err.to_string())
            }
        }
    }

    /// Metadata of `path` as the backend reports it (revision and media type
    /// included), after the read capability check. Only the capability
    /// refusal is `PermissionDenied`; backend errors are returned unchanged.
    pub(crate) fn stat_metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_stat",
            simulacra.operation.name = "sandbox_fs_proxy_stat",
        )
        .entered();

        self.check_read(path, "stat")
            .map_err(VfsError::PermissionDenied)?;
        match self.vfs.metadata(path) {
            Ok(meta) => {
                self.journal_result(
                    "stat",
                    format!(
                        "stat {}: file={}, dir={}, size={}",
                        path, meta.is_file, meta.is_dir, meta.size
                    ),
                    false,
                );
                Ok(meta)
            }
            Err(err) => {
                self.journal_result("stat", err.to_string(), true);
                Err(err)
            }
        }
    }
}

impl FsProxy for AgentCellFsProxy {
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        read_file_inner(
            path,
            &self.vfs,
            &self.capability,
            &self.budget,
            &self.journal,
            &self.agent_id,
        )
        .map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        write_file_inner(
            path,
            data,
            &self.vfs,
            &self.capability,
            &self.budget,
            &self.journal,
            &self.agent_id,
        )
        .map_err(|e| e.to_string())
    }

    fn append_file(&self, path: &str, data: &[u8]) -> Result<(), String> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_append_file",
            simulacra.operation.name = "sandbox_fs_proxy_append_file",
            simulacra.vfs.bytes = data.len() as u64,
        )
        .entered();

        self.check_write(path, "append_file")?;
        self.reserve_write_budget(data.len())?;
        self.journal_file_write(path, data.len());

        if let Err(err) = append_conditionally(self.vfs.as_ref(), path, data) {
            self.release_write_budget(data.len())?;
            return Err(err);
        }
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        <Self as FsProxy>::list_dir_bounded(self, path).map(|(entries, _)| entries)
    }

    fn list_dir_bounded(&self, path: &str) -> Result<(Vec<String>, bool), String> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_list_dir",
            simulacra.operation.name = "sandbox_fs_proxy_list_dir",
        )
        .entered();

        self.check_read(path, "list_dir")?;
        let listing = self.finish("list_dir", self.vfs.list_dir_bounded(path), |listing| {
            format!("listed {} entries in {}", listing.entries.len(), path)
        })?;
        Ok((listing.entries, listing.truncated))
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let meta = self.stat_metadata(path).map_err(|e| e.to_string())?;
        Ok((meta.is_file, meta.is_dir, meta.size))
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_remove",
            simulacra.operation.name = "sandbox_fs_proxy_remove",
        )
        .entered();

        self.check_write(path, "remove")?;
        self.reserve_write_budget(0)?;
        self.finish("remove", self.vfs.remove(path), |_| {
            format!("removed {path}")
        })
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_rename",
            simulacra.operation.name = "sandbox_fs_proxy_rename",
        )
        .entered();

        self.check_write(from, "rename")?;
        self.check_write(to, "rename")?;
        self.reserve_write_budget(0)?;

        self.finish(
            "rename",
            rename_replacing(self.vfs.as_ref(), from, to),
            |_| format!("renamed {from} to {to}"),
        )
    }

    fn exists(&self, path: &str) -> Result<bool, String> {
        check_and_journal_capability(
            || self.capability.check_path_read(path),
            "exists",
            cap_name_for_read(path),
            &self.journal,
            &self.agent_id,
        )
        .map_err(|e| e.to_string())?;

        Ok(self.vfs.exists(path))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_mkdir",
            simulacra.operation.name = "sandbox_fs_proxy_mkdir",
        )
        .entered();

        self.check_write(path, "mkdir")?;
        self.reserve_write_budget(0)?;
        self.finish("mkdir", self.vfs.mkdir(path), |_| {
            format!("created directory {path}")
        })
    }
}
