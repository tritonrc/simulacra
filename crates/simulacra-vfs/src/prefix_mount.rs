//! `PrefixMountFs`: one filesystem answers for a top-level prefix (`/skills`),
//! another for everything else. Paths reach either side unchanged.
//!
//! A rename or batch that would span both sides, or that removes or moves `/`
//! (and with it the mount), is refused as permission denied. Not `Unsupported`:
//! callers emulate an unsupported rename or batch with separate writes and
//! removes, which across two filesystems would leave half a move behind.
//!
//! `subscribe` follows the side its prefix falls on; a watcher on `/` sees the
//! base only.

use std::sync::Arc;

use simulacra_types::{
    DirListing, FsMetadata, FsMutation, Precondition, VfsError, VfsSnapshot, VfsWatcher, VirtualFs,
};

use crate::atomic_paths::{self, Touch};
use crate::path::normalize;

pub struct PrefixMountFs {
    base: Arc<dyn VirtualFs>,
    mounted: Arc<dyn VirtualFs>,
    prefix: String,
    name: String,
    listing_limit: Option<usize>,
}

impl PrefixMountFs {
    /// `prefix` is one segment below `/`, such as `/skills`.
    pub fn new(base: Arc<dyn VirtualFs>, prefix: &str, mounted: Arc<dyn VirtualFs>) -> Self {
        let prefix = normalize(prefix);
        let name = prefix.trim_start_matches('/').to_owned();
        assert!(
            !name.is_empty() && !name.contains('/'),
            "PrefixMountFs mounts at a top-level path, got {prefix}"
        );
        Self {
            base,
            mounted,
            prefix,
            name,
            listing_limit: None,
        }
    }

    /// The most entries one root listing may return, the base's own bound.
    /// The mount's entry counts against it, so the limit must leave it room.
    pub fn with_listing_limit(mut self, limit: usize) -> Self {
        assert!(
            limit >= 1,
            "a root listing must have room for the mount's entry"
        );
        self.listing_limit = Some(limit);
        self
    }

    fn in_mount(&self, normalized: &str) -> bool {
        normalized == self.prefix || normalized.starts_with(&format!("{}/", self.prefix))
    }

    fn fs(&self, normalized: &str) -> &Arc<dyn VirtualFs> {
        if self.in_mount(normalized) {
            &self.mounted
        } else {
            &self.base
        }
    }

    fn denied(&self, what: &str) -> VfsError {
        VfsError::PermissionDenied(format!("{what} crosses the {} mount", self.prefix))
    }

    /// The one side every touched path falls on. A touch that takes its
    /// subtree with it and contains the mount (only `/` can) is refused too.
    fn one_side(&self, what: &str, touches: &[Touch]) -> Result<&Arc<dyn VirtualFs>, VfsError> {
        if touches
            .iter()
            .any(|t| t.takes_subtree && atomic_paths::is_ancestor(&t.path, &self.prefix))
        {
            return Err(self.denied(what));
        }
        let mut sides = touches.iter().map(|t| self.in_mount(&t.path));
        let first = sides.next().unwrap_or(false);
        if sides.all(|side| side == first) {
            Ok(if first { &self.mounted } else { &self.base })
        } else {
            Err(self.denied(what))
        }
    }

    /// The mount point is a directory (see `metadata`), so a mounted filesystem
    /// with no entry for it lists as empty; any other failure is real.
    fn empty_if_absent<T>(listing: Result<T, VfsError>, empty: T) -> Result<T, VfsError> {
        match listing {
            Err(VfsError::NotFound(_)) => Ok(empty),
            other => other,
        }
    }

    /// The base's root entries plus the mount's, within the listing limit.
    /// Returns whether a base entry was displaced to make room.
    fn with_mount_entry(&self, mut names: Vec<String>) -> (Vec<String>, bool) {
        names.retain(|n| n != &self.name);
        let mut displaced = false;
        if let Some(limit) = self.listing_limit {
            let room = limit.saturating_sub(1);
            if names.len() > room {
                names.truncate(room);
                displaced = true;
            }
        }
        names.push(self.name.clone());
        (names, displaced)
    }
}

impl VirtualFs for PrefixMountFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let path = normalize(path);
        self.fs(&path).read(&path)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let path = normalize(path);
        self.fs(&path).write(&path, data)
    }

    fn exists(&self, path: &str) -> bool {
        let path = normalize(path);
        path == self.prefix || self.fs(&path).exists(&path)
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, VfsError> {
        let path = normalize(path);
        if atomic_paths::is_root(&path) {
            return Ok(self.with_mount_entry(self.base.list_dir(&path)?).0);
        }
        let listing = self.fs(&path).list_dir(&path);
        if path == self.prefix {
            return Self::empty_if_absent(listing, Vec::new());
        }
        listing
    }

    fn list_dir_bounded(&self, path: &str) -> Result<DirListing, VfsError> {
        let path = normalize(path);
        if atomic_paths::is_root(&path) {
            let listing = self.base.list_dir_bounded(&path)?;
            let (entries, displaced) = self.with_mount_entry(listing.entries);
            return Ok(DirListing {
                entries,
                truncated: listing.truncated || displaced,
            });
        }
        let listing = self.fs(&path).list_dir_bounded(&path);
        if path == self.prefix {
            return Self::empty_if_absent(
                listing,
                DirListing {
                    entries: Vec::new(),
                    truncated: false,
                },
            );
        }
        listing
    }

    fn mkdir(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        self.fs(&path).mkdir(&path)
    }

    fn remove(&self, path: &str) -> Result<(), VfsError> {
        let path = normalize(path);
        self.one_side("remove", &atomic_paths::removed(&path))?
            .remove(&path)
    }

    fn metadata(&self, path: &str) -> Result<FsMetadata, VfsError> {
        let path = normalize(path);
        if path == self.prefix {
            // The mount point is a directory whether or not the mounted
            // filesystem keeps an entry for it; any other failure is real.
            return match self.mounted.metadata(&path) {
                Err(VfsError::NotFound(_)) => Ok(FsMetadata::dir()),
                other => other,
            };
        }
        self.fs(&path).metadata(&path)
    }

    // The mount is rebuilt by its host, not persisted, so a snapshot is the base's.
    fn snapshot(&self) -> Result<VfsSnapshot, VfsError> {
        self.base.snapshot()
    }

    fn restore(&self, snapshot: &VfsSnapshot) -> Result<(), VfsError> {
        self.base.restore(snapshot)
    }

    fn subscribe(&self, prefix: &str) -> VfsWatcher {
        let prefix = normalize(prefix);
        self.fs(&prefix).subscribe(&prefix)
    }

    fn write_if(
        &self,
        path: &str,
        data: &[u8],
        precondition: Precondition,
    ) -> Result<(), VfsError> {
        let path = normalize(path);
        self.fs(&path).write_if(&path, data, precondition)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        let (from, to) = (normalize(from), normalize(to));
        self.one_side("rename", &atomic_paths::renamed(&from, &to))?
            .rename(&from, &to)
    }

    fn apply_batch(&self, mutations: &[FsMutation]) -> Result<(), VfsError> {
        let mutations = atomic_paths::normalize_mutations(mutations);
        self.one_side("batch", &atomic_paths::batched(&mutations))?
            .apply_batch(&mutations)
    }
}
