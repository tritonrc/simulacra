//! Local-disk and trait-stub implementations of `ArtifactStore`.

use std::fs;
use std::path::{Path, PathBuf};

use simulacra_types::{ArtifactEntry, ArtifactError, ArtifactStore};

/// Validates that an artifact path is safe for filesystem use.
///
/// Rejects empty strings, `..` components, absolute paths, and null bytes.
fn validate_artifact_path(path: &str) -> Result<(), ArtifactError> {
    if path.is_empty() {
        return Err(ArtifactError::InvalidPath("path is empty".into()));
    }
    if path.contains('\0') {
        return Err(ArtifactError::InvalidPath("path contains null byte".into()));
    }
    if path.starts_with('/') {
        return Err(ArtifactError::InvalidPath(
            "absolute paths are not allowed".into(),
        ));
    }
    // Check for `..` as a path component.
    for component in Path::new(path).components() {
        if let std::path::Component::ParentDir = component {
            return Err(ArtifactError::InvalidPath(
                "path contains '..' component".into(),
            ));
        }
    }
    Ok(())
}

/// Local-disk artifact store. Layout: `{root}/{tenant}/{task_id}/{path}`.
pub struct LocalDiskArtifactStore {
    root: PathBuf,
}

impl LocalDiskArtifactStore {
    /// Create a new store, creating the root directory if it does not exist.
    pub fn new(root: &Path) -> Result<Self, ArtifactError> {
        fs::create_dir_all(root)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Resolve the task directory scoped to a specific tenant.
    fn task_dir(&self, tenant: &str, task_id: &str) -> PathBuf {
        self.root.join(tenant).join(task_id)
    }

    /// Recursively collect all files under `dir`, returning paths relative to `base`.
    fn walk_files(base: &Path, dir: &Path) -> Result<Vec<ArtifactEntry>, ArtifactError> {
        let mut results = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                results.extend(Self::walk_files(base, &path)?);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .expect("walk_files: path must be under base");
                let metadata = fs::metadata(&path)?;
                results.push(ArtifactEntry {
                    path: rel.to_string_lossy().into_owned(),
                    size: metadata.len(),
                });
            }
        }
        Ok(results)
    }
}

impl ArtifactStore for LocalDiskArtifactStore {
    fn put(
        &self,
        task_id: &str,
        tenant: &str,
        path: &str,
        data: &[u8],
    ) -> Result<(), ArtifactError> {
        validate_artifact_path(path)?;

        let full_path = self.root.join(tenant).join(task_id).join(path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Atomic write: write to unique temp file in the same directory, then rename.
        // Unique name avoids collisions between concurrent writers to the same path.
        let file_name = full_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".into());
        let unique = uuid::Uuid::new_v4();
        let tmp_path = full_path.with_file_name(format!(".{file_name}.{unique}.tmp"));

        fs::write(&tmp_path, data)?;
        fs::rename(&tmp_path, &full_path)?;

        Ok(())
    }

    fn get(&self, tenant: &str, task_id: &str, path: &str) -> Result<Vec<u8>, ArtifactError> {
        validate_artifact_path(path)?;
        let task_dir = self.task_dir(tenant, task_id);
        if !task_dir.is_dir() {
            return Err(ArtifactError::NotFound(format!("{task_id}/{path}")));
        }

        let full_path = task_dir.join(path);
        if !full_path.is_file() {
            return Err(ArtifactError::NotFound(format!("{task_id}/{path}")));
        }

        Ok(fs::read(&full_path)?)
    }

    fn list(&self, tenant: &str, task_id: &str) -> Result<Vec<ArtifactEntry>, ArtifactError> {
        let task_dir = self.task_dir(tenant, task_id);
        if !task_dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut entries = Self::walk_files(&task_dir, &task_dir)?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    fn delete_task(&self, tenant: &str, task_id: &str) -> Result<(), ArtifactError> {
        let task_dir = self.task_dir(tenant, task_id);
        if task_dir.is_dir() {
            fs::remove_dir_all(&task_dir)?;
        }
        Ok(())
    }
}

/// Marker trait for future S3-backed artifact storage.
///
/// Interface only — implementation is a future spec.
pub trait S3ArtifactStore: Send + Sync + 'static {}
