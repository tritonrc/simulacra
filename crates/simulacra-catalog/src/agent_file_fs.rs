//! S045 — read-only `VirtualFs` exposing per-agent files at the mount root.
//!
//! Bytes are pre-loaded at task spawn so the layer honors snapshot
//! semantics: files detached from the catalog after spawn don't disappear
//! from a running task's VFS. The owning [`crate::AgentFileStore`] is
//! free to purge bytes on detach because this layer holds its own copy.

use std::sync::Arc;

use simulacra_types::{FsMetadata, VfsError, VfsSnapshot, VirtualFs};

use crate::models::AgentFile;

/// Read-only VFS over a snapshot of `(AgentFile, bytes)` pairs. Mounted at
/// `/var/agent_files/` by the engine; from the layer's point of view the
/// mount root is `/`.
pub struct CatalogAgentFileFs {
    files: Arc<Vec<(AgentFile, Vec<u8>)>>,
}

impl CatalogAgentFileFs {
    pub fn new(files: Vec<(AgentFile, Vec<u8>)>) -> Self {
        Self {
            files: Arc::new(files),
        }
    }

    fn lookup(&self, path: &str) -> Option<&(AgentFile, Vec<u8>)> {
        let stripped = path.strip_prefix('/').unwrap_or(path);
        if stripped.is_empty() {
            return None;
        }
        self.files.iter().find(|(meta, _)| meta.name == stripped)
    }
}

impl VirtualFs for CatalogAgentFileFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        match self.lookup(path) {
            Some((_, bytes)) => Ok(bytes.clone()),
            None => Err(VfsError::NotFound(path.to_owned())),
        }
    }

    fn write(&self, path: &str, _data: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(format!(
            "agent files are read-only: {path}"
        )))
    }

    fn exists(&self, path: &str) -> bool {
        if path == "/" || path.is_empty() {
            return true;
        }
        self.lookup(path).is_some()
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        if path == "/" || path.is_empty() {
            Ok(self
                .files
                .iter()
                .map(|(meta, _)| meta.name.clone())
                .collect())
        } else {
            Err(VfsError::NotFound(path.to_owned()))
        }
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(format!(
            "agent files are read-only: {path}"
        )))
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(format!(
            "agent files are read-only: {path}"
        )))
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        if path == "/" || path.is_empty() {
            return Ok(FsMetadata {
                is_file: false,
                is_dir: true,
                size: 0,
            });
        }
        match self.lookup(path) {
            Some((_, bytes)) => Ok(FsMetadata {
                is_file: true,
                is_dir: false,
                size: bytes.len() as u64,
            }),
            None => Err(VfsError::NotFound(path.to_owned())),
        }
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        Ok(VfsSnapshot { data: Vec::new() })
    }

    fn restore(&self, _snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(
            "agent files are read-only".into(),
        ))
    }
}
