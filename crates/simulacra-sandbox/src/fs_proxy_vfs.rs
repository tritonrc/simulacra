//! `AgentCellFsProxy`'s own `VirtualFs` impl (what the mediated shell gets).
//!
//! Plain methods go through the [`FsProxy`] methods. The atomic ones forward
//! to the backend after the same capability checks and with the same budget
//! and journal accounting. Capability and budget refusals are
//! `PermissionDenied`; errors from the backend are returned as they are.

use crate::fs_proxy::AgentCellFsProxy;
use simulacra_quickjs::FsProxy;
use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot, VfsWatcher, VirtualFs,
};

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
        // The backend's own metadata: callers build preconditions on its
        // revision.
        self.stat_metadata(path)
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

    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: Precondition,
    ) -> Result<(), VfsError> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_write_if",
            simulacra.operation.name = "sandbox_fs_proxy_write_if",
            simulacra.vfs.bytes = data.len() as u64,
        )
        .entered();

        self.check_write(path, "write_if")
            .map_err(sandbox_error_to_vfs)?;
        self.reserve_write_budget(data.len())
            .map_err(sandbox_error_to_vfs)?;
        self.journal_file_write(path, data.len());
        if let Err(err) = self.vfs.write_if(path, data, precondition) {
            self.release_write_budget(&err, data.len())
                .map_err(sandbox_error_to_vfs)?;
            return Err(err);
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_rename",
            simulacra.operation.name = "sandbox_fs_proxy_rename",
        )
        .entered();

        self.check_write(from, "rename")
            .map_err(sandbox_error_to_vfs)?;
        self.check_write(to, "rename")
            .map_err(sandbox_error_to_vfs)?;
        self.reserve_write_budget(0).map_err(sandbox_error_to_vfs)?;
        match self.vfs.rename(from, to) {
            // Nothing was attempted; the caller emulates through the plain
            // methods, which journal themselves.
            Err(err @ VfsError::Unsupported(_)) => Err(err),
            Err(err) => {
                self.journal_result("rename", err.to_string(), true);
                Err(err)
            }
            Ok(()) => {
                self.journal_result("rename", format!("renamed {from} to {to}"), false);
                Ok(())
            }
        }
    }

    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        let writes = || {
            mutations.iter().filter_map(|m| match m {
                FsMutation::Write { path, data, .. } => Some((path, data.len())),
                _ => None,
            })
        };
        let total: usize = writes().map(|(_, len)| len).sum();
        let _span = tracing::info_span!(
            "sandbox_fs_proxy_apply_batch",
            simulacra.operation.name = "sandbox_fs_proxy_apply_batch",
            simulacra.vfs.bytes = total as u64,
        )
        .entered();

        // Every touched path is judged before anything is applied.
        for mutation in mutations {
            for path in mutation.paths() {
                self.check_write(path, "apply_batch")
                    .map_err(sandbox_error_to_vfs)?;
            }
        }
        self.reserve_write_budget(total)
            .map_err(sandbox_error_to_vfs)?;
        for (path, len) in writes() {
            self.journal_file_write(path, len);
        }
        match self.vfs.apply_batch(mutations) {
            Ok(()) => {
                self.journal_result(
                    "apply_batch",
                    format!("applied {} mutations", mutations.len()),
                    false,
                );
                Ok(())
            }
            Err(err) => {
                self.release_write_budget(&err, total)
                    .map_err(sandbox_error_to_vfs)?;
                // `Unsupported` attempted nothing; the caller emulates.
                if !matches!(err, VfsError::Unsupported(_)) {
                    self.journal_result("apply_batch", err.to_string(), true);
                }
                Err(err)
            }
        }
    }

    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        let (entries, truncated) =
            <Self as FsProxy>::list_dir_bounded(self, path).map_err(sandbox_error_to_vfs)?;
        Ok(DirListing { entries, truncated })
    }
}
