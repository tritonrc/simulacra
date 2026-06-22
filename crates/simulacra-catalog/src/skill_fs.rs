use std::sync::Arc;

use simulacra_types::{FsMetadata, VfsError, VfsSnapshot, VirtualFs};

use crate::models::Skill;

/// Read-only `VirtualFs` that exposes a snapshot of catalog skills as
/// `<name>.md` files at the mount root.
///
/// Composed alongside any host-mounted skills layer via `OverlayFs`. This
/// layer is read-only; mutating ops return `VfsError::PermissionDenied`.
pub struct CatalogSkillFs {
    skills: Arc<Vec<Skill>>,
}

impl CatalogSkillFs {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills: Arc::new(skills),
        }
    }

    fn render(skill: &Skill) -> String {
        let mut out = String::new();
        if let Some(meta) = &skill.metadata {
            out.push_str("---\n");
            if let Ok(yaml) = serde_yaml::to_string(meta) {
                out.push_str(&yaml);
            }
            out.push_str("---\n\n");
        }
        out.push_str(&skill.body);
        out
    }

    fn lookup_by_path(&self, path: &str) -> Option<&Skill> {
        // Accept "/foo.md" → "foo".
        let stripped = path.strip_prefix('/').unwrap_or(path);
        let name = stripped.strip_suffix(".md")?;
        self.skills.iter().find(|s| s.name == name)
    }
}

impl VirtualFs for CatalogSkillFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        match self.lookup_by_path(path) {
            Some(skill) => Ok(Self::render(skill).into_bytes()),
            None => Err(VfsError::NotFound(path.to_owned())),
        }
    }

    fn write(&self, path: &str, _data: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(format!(
            "catalog skills are read-only: {path}"
        )))
    }

    fn exists(&self, path: &str) -> bool {
        if path == "/" || path.is_empty() {
            return true;
        }
        self.lookup_by_path(path).is_some()
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        if path == "/" || path.is_empty() {
            Ok(self
                .skills
                .iter()
                .map(|s| format!("{}.md", s.name))
                .collect())
        } else {
            Err(VfsError::NotFound(path.to_owned()))
        }
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(format!(
            "catalog skills are read-only: {path}"
        )))
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(format!(
            "catalog skills are read-only: {path}"
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
        match self.lookup_by_path(path) {
            Some(skill) => {
                let rendered = Self::render(skill);
                Ok(FsMetadata {
                    is_file: true,
                    is_dir: false,
                    size: rendered.len() as u64,
                })
            }
            None => Err(VfsError::NotFound(path.to_owned())),
        }
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        // Read-only — empty snapshot. The catalog is the source of truth.
        Ok(VfsSnapshot { data: Vec::new() })
    }

    fn restore(&self, _snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        Err(VfsError::PermissionDenied(
            "catalog skills are read-only".into(),
        ))
    }
}
