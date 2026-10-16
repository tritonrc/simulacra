use std::collections::BTreeMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use simulacra_types::{FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot, VirtualFs};
use tracing::info_span;

use crate::path::normalize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Entry {
    File { data: Vec<u8>, revision: u64 },
    Dir,
}

pub(crate) struct State {
    pub(crate) map: BTreeMap<String, Entry>,
    /// Only increases, so a revision seen once never names different
    /// contents later: not after an overwrite, a delete, a re-create or a
    /// restore.
    pub(crate) next_revision: u64,
}

impl State {
    pub(crate) fn take_revision(&mut self) -> u64 {
        self.next_revision += 1;
        self.next_revision
    }
}

/// Ensure all ancestor directories of `path` exist in `map` (mkdir -p).
pub(crate) fn ensure_parents(map: &mut BTreeMap<String, Entry>, path: &str) {
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
    pub(crate) inner: RwLock<State>,
}

impl MemoryFs {
    /// Create a new empty filesystem with only the root directory.
    pub fn new() -> Self {
        let mut map = BTreeMap::new();
        map.insert("/".to_string(), Entry::Dir);
        Self {
            inner: RwLock::new(State {
                map,
                next_revision: 0,
            }),
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
        let _span = info_span!("vfs_read", simulacra.operation.name = "vfs_read").entered();
        let state = self.inner.read().unwrap();
        match state.map.get(&path) {
            Some(Entry::File { data, .. }) => Ok(data.clone()),
            Some(Entry::Dir) => Err(VfsError::NotAFile(path)),
            None => Err(VfsError::NotFound(path)),
        }
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let path = normalize(path);
        let _span = info_span!("vfs_write", simulacra.operation.name = "vfs_write").entered();
        if path == "/" {
            return Err(VfsError::NotAFile(path));
        }
        let mut state = self.inner.write().unwrap();
        // Implicitly create parent directories.
        ensure_parents(&mut state.map, &path);
        let revision = state.take_revision();
        state.map.insert(
            path,
            Entry::File {
                data: data.to_vec(),
                revision,
            },
        );
        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        let path = normalize(path);
        let state = self.inner.read().unwrap();
        state.map.contains_key(&path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let path = normalize(path);
        let state = self.inner.read().unwrap();
        match state.map.get(&path) {
            Some(Entry::Dir) => {}
            Some(Entry::File { .. }) => return Err(VfsError::NotADirectory(path)),
            None => return Err(VfsError::NotFound(path)),
        }
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        let mut names: Vec<String> = Vec::new();
        for key in state.map.keys() {
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
        let mut state = self.inner.write().unwrap();
        if let Some(entry) = state.map.get(&path) {
            return match entry {
                Entry::Dir => Ok(()),
                Entry::File { .. } => Err(VfsError::NotAFile(path)),
            };
        }
        ensure_parents(&mut state.map, &path);
        state.map.insert(path, Entry::Dir);
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        if path == "/" {
            return Err(VfsError::Io("cannot remove root".to_string()));
        }
        let mut state = self.inner.write().unwrap();
        if !state.map.contains_key(&path) {
            return Err(VfsError::NotFound(path));
        }
        // Remove the entry and any children (if it is a directory).
        let prefix = format!("{path}/");
        let keys_to_remove: Vec<String> = state
            .map
            .keys()
            .filter(|k| *k == &path || k.starts_with(&prefix))
            .cloned()
            .collect();
        for k in keys_to_remove {
            state.map.remove(&k);
        }
        Ok(())
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let path = normalize(path);
        let state = self.inner.read().unwrap();
        match state.map.get(&path) {
            Some(Entry::File { data, revision }) => {
                Ok(FsMetadata::file(data.len() as u64).with_revision(*revision))
            }
            Some(Entry::Dir) => Ok(FsMetadata::dir()),
            None => Err(VfsError::NotFound(path)),
        }
    }

    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        let _span = info_span!("vfs_snapshot", simulacra.operation.name = "vfs_snapshot").entered();
        let state = self.inner.read().unwrap();
        let items: Vec<(String, bool, Vec<u8>)> = state
            .map
            .iter()
            .map(|(k, v)| match v {
                Entry::File { data, .. } => (k.clone(), true, data.clone()),
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
        let mut state = self.inner.write().unwrap();
        state.map.clear();
        for (p, is_file, data) in items {
            if is_file {
                let revision = state.take_revision();
                state.map.insert(p, Entry::File { data, revision });
            } else {
                state.map.insert(p, Entry::Dir);
            }
        }
        Ok(())
    }

    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: Precondition,
    ) -> Result<(), VfsError> {
        let _span = info_span!("vfs_write_if", simulacra.operation.name = "vfs_write_if").entered();
        let mut state = self.inner.write().unwrap();
        // Stricter than plain `write`: a file where a parent directory is
        // needed is `NotADirectory`, not silently given a child.
        crate::memory_atomic::write_if(&mut state, path, data.to_vec(), precondition)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        let mutation = FsMutation::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
            precondition: Precondition::Unconditional,
        };
        self.apply_batch(std::slice::from_ref(&mutation))
    }

    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        let _span = info_span!(
            "vfs_apply_batch",
            simulacra.operation.name = "vfs_apply_batch"
        )
        .entered();
        let mut state = self.inner.write().unwrap();
        crate::memory_atomic::apply(&mut state, mutations)
    }
}
