use std::collections::BTreeMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use simulacra_types::{FsMetadata, VfsError, VfsSnapshot, VirtualFs};
use tracing::info_span;

use crate::path::normalize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Entry {
    File(Vec<u8>),
    Dir,
}

/// Ensure all ancestor directories of `path` exist in `map` (mkdir -p).
fn ensure_parents(map: &mut BTreeMap<String, Entry>, path: &str) {
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur = format!("{cur}/{seg}");
        if cur == path {
            break;
        }
        map.entry(cur.clone()).or_insert(Entry::Dir);
    }
}

/// A fully in-memory virtual filesystem backed by a sorted `BTreeMap`.
///
/// Thread-safe via interior `RwLock`. All paths are absolute and start with `/`.
/// `write()` implicitly creates parent directories (mkdir -p semantics).
pub struct MemoryFs {
    inner: RwLock<BTreeMap<String, Entry>>,
}

impl MemoryFs {
    /// Create a new empty filesystem with only the root directory.
    pub fn new() -> Self {
        let mut map = BTreeMap::new();
        map.insert("/".to_string(), Entry::Dir);
        Self {
            inner: RwLock::new(map),
        }
    }
}

impl Default for MemoryFs {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualFs for MemoryFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let path = normalize(path);
        let _span =
            info_span!("vfs_read", simulacra.operation.name = "vfs_read", simulacra.vfs.path = %path)
                .entered();
        let map = self.inner.read().unwrap();
        match map.get(&path) {
            Some(Entry::File(data)) => Ok(data.clone()),
            Some(Entry::Dir) => Err(VfsError::NotAFile(path)),
            None => Err(VfsError::NotFound(path)),
        }
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let path = normalize(path);
        let _span =
            info_span!("vfs_write", simulacra.operation.name = "vfs_write", simulacra.vfs.path = %path)
                .entered();
        if path == "/" {
            return Err(VfsError::NotAFile(path));
        }
        let mut map = self.inner.write().unwrap();
        // Implicitly create parent directories.
        ensure_parents(&mut map, &path);
        map.insert(path, Entry::File(data.to_vec()));
        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        let path = normalize(path);
        let map = self.inner.read().unwrap();
        map.contains_key(&path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let path = normalize(path);
        let map = self.inner.read().unwrap();
        match map.get(&path) {
            Some(Entry::Dir) => {}
            Some(Entry::File(_)) => return Err(VfsError::NotADirectory(path)),
            None => return Err(VfsError::NotFound(path)),
        }
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        let mut names: Vec<String> = Vec::new();
        for key in map.keys() {
            if key == &path {
                continue;
            }
            if let Some(rest) = key.strip_prefix(&prefix) {
                // Direct children only: no further `/` in the remainder.
                if !rest.contains('/') && !rest.is_empty() {
                    names.push(rest.to_string());
                }
            }
        }
        // BTreeMap is sorted, so names are already sorted.
        Ok(names)
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        let mut map = self.inner.write().unwrap();
        if let Some(entry) = map.get(&path) {
            return match entry {
                Entry::Dir => Ok(()),
                Entry::File(_) => Err(VfsError::NotAFile(path)),
            };
        }
        ensure_parents(&mut map, &path);
        map.insert(path, Entry::Dir);
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        if path == "/" {
            return Err(VfsError::Io("cannot remove root".to_string()));
        }
        let mut map = self.inner.write().unwrap();
        if !map.contains_key(&path) {
            return Err(VfsError::NotFound(path));
        }
        // Remove the entry and any children (if it is a directory).
        let prefix = format!("{path}/");
        let keys_to_remove: Vec<String> = map
            .keys()
            .filter(|k| *k == &path || k.starts_with(&prefix))
            .cloned()
            .collect();
        for k in keys_to_remove {
            map.remove(&k);
        }
        Ok(())
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let path = normalize(path);
        let map = self.inner.read().unwrap();
        match map.get(&path) {
            Some(Entry::File(data)) => Ok(FsMetadata {
                is_file: true,
                is_dir: false,
                size: data.len() as u64,
            }),
            Some(Entry::Dir) => Ok(FsMetadata {
                is_file: false,
                is_dir: true,
                size: 0,
            }),
            None => Err(VfsError::NotFound(path)),
        }
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        let _span = info_span!("vfs_snapshot", simulacra.operation.name = "vfs_snapshot").entered();
        let map = self.inner.read().unwrap();
        let items: Vec<(String, bool, Vec<u8>)> = map
            .iter()
            .map(|(k, v)| match v {
                Entry::File(d) => (k.clone(), true, d.clone()),
                Entry::Dir => (k.clone(), false, Vec::new()),
            })
            .collect();
        let data = serde_json::to_vec(&items).map_err(|e| VfsError::Io(e.to_string()))?;
        Ok(VfsSnapshot { data })
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        let _span = info_span!("vfs_restore", simulacra.operation.name = "vfs_restore").entered();
        let items: Vec<(String, bool, Vec<u8>)> =
            serde_json::from_slice(&snapshot.data).map_err(|e| VfsError::Io(e.to_string()))?;
        let mut map = self.inner.write().unwrap();
        map.clear();
        for (p, is_file, data) in items {
            if is_file {
                map.insert(p, Entry::File(data));
            } else {
                map.insert(p, Entry::Dir);
            }
        }
        Ok(())
    }
}
