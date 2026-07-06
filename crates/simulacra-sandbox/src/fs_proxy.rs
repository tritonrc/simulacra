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
    JournalStorage, ResourceBudget, VfsError, VfsSnapshot, VfsWatcher, VirtualFs,
};
use std::sync::{Arc, Mutex};

use crate::file_io::{read_file_inner, write_file_inner};
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
    fn check_read(&self, path: &str, operation: &str) -> Result<(), String> {
        check_and_journal_capability(
            || self.capability.check_path_read(path),
            operation,
            cap_name_for_read(path),
            &self.journal,
            &self.agent_id,
        )
        .map_err(|e| e.to_string())
    }

    fn check_write(&self, path: &str, operation: &str) -> Result<(), String> {
        check_and_journal_capability(
            || self.capability.check_path_write(path),
            operation,
            cap_name_for_write(path),
            &self.journal,
            &self.agent_id,
        )
        .map_err(|e| e.to_string())
    }

    fn reserve_write_budget(&self, bytes: usize) -> Result<(), String> {
        reserve_vfs_bytes(&self.budget, bytes as u64, &self.journal, &self.agent_id)
            .map_err(|e| e.to_string())
    }

    fn release_write_budget(&self, bytes: usize) -> Result<(), String> {
        release_vfs_bytes(&self.budget, bytes as u64).map_err(|e| e.to_string())
    }

    fn journal_file_write(&self, path: &str, size_bytes: usize) {
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::FileWrite {
                path: path.to_string(),
                size_bytes: size_bytes as u64,
            },
        }) {
            tracing::error!(error = %err, path, "journal append failed for fs proxy file write");
        }
    }

    fn journal_result(&self, operation: &str, content: String, is_error: bool) {
        if let Err(err) = self.journal.append(JournalEntry {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_id: self.agent_id.clone(),
            timestamp_ms: 0,
            entry: JournalEntryKind::ToolResult {
                tool_call_id: None,

                tool_name: operation.to_string(),
                content,
                is_error,
            },
        }) {
            tracing::error!(error = %err, operation, "journal append failed for fs proxy");
        }
    }

    fn finish<T>(
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
            simulacra.vfs.path = path,
            simulacra.vfs.bytes = data.len() as u64,
        )
        .entered();

        self.check_write(path, "append_file")?;
        self.reserve_write_budget(data.len())?;
        self.journal_file_write(path, data.len());

        let mut combined = match self.vfs.read(path) {
            Ok(existing) => existing,
            Err(VfsError::NotFound(_)) => Vec::new(),
            Err(err) => {
                self.release_write_budget(data.len())?;
                return Err(err.to_string());
            }
        };
        combined.extend_from_slice(data);
        if let Err(err) = self.vfs.write(path, &combined) {
            self.release_write_budget(data.len())?;
            return Err(err.to_string());
        }
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_list_dir",
            simulacra.operation.name = "sandbox_fs_proxy_list_dir",
            simulacra.vfs.path = path,
        )
        .entered();

        self.check_read(path, "list_dir")?;
        self.finish("list_dir", self.vfs.list_dir(path), |entries| {
            format!("listed {} entries in {}", entries.len(), path)
        })
    }

    fn stat(&self, path: &str) -> Result<(bool, bool, u64), String> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_stat",
            simulacra.operation.name = "sandbox_fs_proxy_stat",
            simulacra.vfs.path = path,
        )
        .entered();

        self.check_read(path, "stat")?;
        let meta = self.finish("stat", self.vfs.metadata(path), |meta| {
            format!(
                "stat {}: file={}, dir={}, size={}",
                path, meta.is_file, meta.is_dir, meta.size
            )
        })?;
        Ok((meta.is_file, meta.is_dir, meta.size))
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_remove",
            simulacra.operation.name = "sandbox_fs_proxy_remove",
            simulacra.vfs.path = path,
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
            simulacra.vfs.path = from,
            simulacra.vfs.destination = to,
        )
        .entered();

        self.check_write(from, "rename")?;
        self.check_write(to, "rename")?;
        self.reserve_write_budget(0)?;

        if let Err(err) = copy_vfs_entry(self.vfs.as_ref(), from, to) {
            self.journal_result("rename", err.to_string(), true);
            return Err(err.to_string());
        }
        self.finish("rename", self.vfs.remove(from), |_| {
            format!("renamed {from} to {to}")
        })
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
            simulacra.vfs.path = path,
        )
        .entered();

        self.check_write(path, "mkdir")?;
        self.reserve_write_budget(0)?;
        self.finish("mkdir", self.vfs.mkdir(path), |_| {
            format!("created directory {path}")
        })
    }
}

fn join_vfs_path(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), child)
    }
}

fn copy_vfs_entry(vfs: &dyn VirtualFs, from: &str, to: &str) -> Result<(), VfsError> {
    let meta = vfs.metadata(from)?;
    if meta.is_file {
        let data = vfs.read(from)?;
        return vfs.write(to, &data);
    }

    vfs.mkdir(to)?;
    for child in vfs.list_dir(from)? {
        let child_from = join_vfs_path(from, &child);
        let child_to = join_vfs_path(to, &child);
        copy_vfs_entry(vfs, &child_from, &child_to)?;
    }
    Ok(())
}

fn sandbox_error_to_vfs(error: String) -> VfsError {
    VfsError::PermissionDenied(error)
}

impl VirtualFs for AgentCellFsProxy {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        <Self as FsProxy>::read_file(self, path).map_err(sandbox_error_to_vfs)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        <Self as FsProxy>::write_file(self, path, data).map_err(sandbox_error_to_vfs)
    }

    fn exists(&self, path: &str) -> bool {
        <Self as FsProxy>::exists(self, path).unwrap_or(false)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        <Self as FsProxy>::list_dir(self, path).map_err(sandbox_error_to_vfs)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        <Self as FsProxy>::mkdir(self, path).map_err(sandbox_error_to_vfs)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        <Self as FsProxy>::remove(self, path).map_err(sandbox_error_to_vfs)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let (is_file, is_dir, size) =
            <Self as FsProxy>::stat(self, path).map_err(sandbox_error_to_vfs)?;
        Ok(FsMetadata {
            is_file,
            is_dir,
            size,
        })
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        Err(VfsError::PermissionDenied(
            "snapshot is not available through mediated shell fs".into(),
        ))
    }

    fn restore(&self, _snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(
            "restore is not available through mediated shell fs".into(),
        ))
    }

    fn subscribe(&self, prefix: &str) -> VfsWatcher {
        self.vfs.subscribe(prefix)
    }
}
