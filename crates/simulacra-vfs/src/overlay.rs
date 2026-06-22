use std::collections::HashSet;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use simulacra_types::{FsMetadata, VfsError, VfsSnapshot, VirtualFs};
use tracing::info_span;

use crate::path::normalize;

/// A copy-on-write overlay combining a read-only lower layer and a
/// read-write upper layer. Deletions in the upper layer shadow the lower
/// layer via a whiteout set.
pub struct OverlayFs {
    lower: Box<dyn VirtualFs>,
    upper: Box<dyn VirtualFs>,
    whiteouts: RwLock<HashSet<String>>,
}

impl OverlayFs {
    pub fn new(lower: Box<dyn VirtualFs>, upper: Box<dyn VirtualFs>) -> Self {
        Self {
            lower,
            upper,
            whiteouts: RwLock::new(HashSet::new()),
        }
    }

    fn is_whited_out(&self, path: &str) -> bool {
        let guard = self.whiteouts.read().unwrap();
        if guard.contains(path) {
            return true;
        }
        // Check if any ancestor is whited out (directory removal shadows children).
        let mut current = path;
        while let Some(pos) = current.rfind('/') {
            if pos == 0 {
                return guard.contains("/");
            }
            current = &current[..pos];
            if guard.contains(current) {
                return true;
            }
        }
        false
    }

    fn clear_whiteouts_recursive(&self, path: &str) {
        let mut guard = self.whiteouts.write().unwrap();
        guard.remove(path);

        // Also clear ancestors, because writing a file implies reviving its parents (mkdir -p).
        let mut current = path;
        while let Some(pos) = current.rfind('/') {
            if pos == 0 {
                guard.remove("/");
                break;
            }
            current = &current[..pos];
            guard.remove(current);
        }
    }
}

impl VirtualFs for OverlayFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let path = normalize(path);
        let _span =
            info_span!("vfs_read", simulacra.operation.name = "vfs_read", simulacra.vfs.path = %path)
                .entered();

        if self.is_whited_out(&path) {
            return Err(VfsError::NotFound(path));
        }
        match self.upper.read(&path) {
            Ok(data) => Ok(data),
            Err(VfsError::NotFound(_)) => self.lower.read(&path),
            Err(e) => Err(e),
        }
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let path = normalize(path);
        let _span =
            info_span!("vfs_write", simulacra.operation.name = "vfs_write", simulacra.vfs.path = %path)
                .entered();

        // Clear whiteout (and ancestor whiteouts) so the file becomes visible again.
        self.clear_whiteouts_recursive(&path);
        self.upper.write(&path, data)
    }

    fn exists(&self, path: &str) -> bool {
        let path = normalize(path);
        if self.is_whited_out(&path) {
            return false;
        }
        self.upper.exists(&path) || self.lower.exists(&path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let path = normalize(path);
        // list_dir doesn't explicitly require a span in spec, but good practice?
        // Spec: write, read, snapshot, restore require spans. list_dir is not listed in observability.

        if self.is_whited_out(&path) {
            return Err(VfsError::NotFound(path));
        }

        let upper_exists = self.upper.exists(&path);
        let lower_exists = self.lower.exists(&path);

        if !upper_exists && !lower_exists {
            return Err(VfsError::NotFound(path));
        }

        let mut names: HashSet<String> = HashSet::new();

        if lower_exists && let Ok(entries) = self.lower.list_dir(&path) {
            let prefix = if path == "/" {
                "/".to_string()
            } else {
                format!("{path}/")
            };
            for e in entries {
                let full = format!("{prefix}{e}");
                if !self.is_whited_out(&full) {
                    names.insert(e);
                }
            }
        }

        if upper_exists && let Ok(entries) = self.upper.list_dir(&path) {
            for e in entries {
                names.insert(e);
            }
        }

        let mut sorted: Vec<String> = names.into_iter().collect();
        sorted.sort();
        Ok(sorted)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        // No span required by spec for mkdir.

        self.clear_whiteouts_recursive(&path);
        self.upper.mkdir(&path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        // No span required by spec for remove.

        if !self.exists(&path) {
            return Err(VfsError::NotFound(path));
        }
        // Remove from upper if present.
        let _ = self.upper.remove(&path);
        // Add whiteout to shadow lower.
        self.whiteouts.write().unwrap().insert(path);
        Ok(())
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let path = normalize(path);
        if self.is_whited_out(&path) {
            return Err(VfsError::NotFound(path));
        }
        match self.upper.metadata(&path) {
            Ok(m) => Ok(m),
            Err(VfsError::NotFound(_)) => self.lower.metadata(&path),
            Err(e) => Err(e),
        }
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        let _span = info_span!("vfs_snapshot", simulacra.operation.name = "vfs_snapshot").entered();

        let upper_snap = self.upper.snapshot()?;
        let whiteouts: Vec<String> = self.whiteouts.read().unwrap().iter().cloned().collect();
        let combined = OverlaySnapshot {
            upper: upper_snap.data,
            whiteouts,
        };
        let json = serde_json::to_vec(&combined).map_err(|e| VfsError::Io(e.to_string()))?;
        Ok(VfsSnapshot { data: json })
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        let _span = info_span!("vfs_restore", simulacra.operation.name = "vfs_restore").entered();

        let combined: OverlaySnapshot =
            serde_json::from_slice(&snapshot.data).map_err(|e| VfsError::Io(e.to_string()))?;
        let upper_snap = VfsSnapshot {
            data: combined.upper,
        };
        self.upper.restore(&upper_snap)?;
        let mut wo = self.whiteouts.write().unwrap();
        *wo = combined.whiteouts.into_iter().collect();
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct OverlaySnapshot {
    upper: Vec<u8>,
    whiteouts: Vec<String>,
}
