use std::sync::Arc;

use serde_json::{Map, Value};
use simulacra_types::{FsMetadata, VfsError, VfsSnapshot, VirtualFs};

use crate::models::Skill;

/// Read-only `VirtualFs` that exposes a snapshot of catalog skills as
/// `<name>/SKILL.md` directories at the mount root.
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

    pub fn is_valid_skill_path_name(name: &str) -> bool {
        is_valid_skill_path_name(name)
    }

    fn render(skill: &Skill) -> String {
        let mut frontmatter = match skill.metadata.as_ref() {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        frontmatter.insert("name".to_string(), Value::String(skill.name.clone()));
        let description = skill
            .description
            .as_deref()
            .filter(|description| !description.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                frontmatter
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|description| !description.trim().is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| skill.name.clone());
        frontmatter.insert("description".to_string(), Value::String(description));

        let mut out = String::new();
        out.push_str("---\n");
        if let Ok(yaml) = serde_yaml::to_string(&Value::Object(frontmatter)) {
            out.push_str(&yaml);
        }
        out.push_str("---\n\n");
        out.push_str(&skill.body);
        out
    }

    fn lookup_file(&self, path: &str) -> Option<&Skill> {
        let stripped = path.strip_prefix('/').unwrap_or(path);
        let mut components = stripped.split('/');
        let name = components.next()?;
        let file = components.next()?;
        if components.next().is_some() || file != "SKILL.md" || !is_valid_skill_path_name(name) {
            return None;
        }
        self.skills.iter().find(|s| s.name == name)
    }

    fn lookup_dir(&self, path: &str) -> Option<&Skill> {
        let stripped = path.strip_prefix('/').unwrap_or(path).trim_end_matches('/');
        if stripped.is_empty() || stripped.contains('/') {
            return None;
        }
        if !is_valid_skill_path_name(stripped) {
            return None;
        }
        self.skills.iter().find(|s| s.name == stripped)
    }
}

fn is_valid_skill_path_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

impl VirtualFs for CatalogSkillFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        match self.lookup_file(path) {
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
        self.lookup_file(path).is_some() || self.lookup_dir(path).is_some()
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        if path == "/" || path.is_empty() {
            let mut entries: Vec<String> = self
                .skills
                .iter()
                .filter(|s| is_valid_skill_path_name(&s.name))
                .map(|s| s.name.clone())
                .collect();
            entries.sort();
            Ok(entries)
        } else if self.lookup_dir(path).is_some() {
            Ok(vec!["SKILL.md".to_string()])
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
        if self.lookup_dir(path).is_some() {
            return Ok(FsMetadata {
                is_file: false,
                is_dir: true,
                size: 0,
            });
        }

        match self.lookup_file(path) {
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
