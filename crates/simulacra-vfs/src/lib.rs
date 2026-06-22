//! In-memory and overlay virtual filesystem implementations.
//!
//! Provides [`MemoryFs`] (a BTreeMap-backed VFS), [`OverlayFs`]
//! (a copy-on-write layer over a read-only base), and [`ProcFs`]
//! (a virtual `/proc` directory exposing agent runtime state).
//! All implement [`simulacra_types::VirtualFs`].

pub mod mailboxfs;
mod memory;
mod memory_store_fs;
pub mod mount;
mod notifying;
mod overlay;
mod path;
pub mod procfs;
mod readonly_path_guard;
pub mod servicefs;

pub use mailboxfs::{ArtifactWriteSink, MailboxFs};
pub use memory::MemoryFs;
pub use memory_store_fs::MemoryStoreFs;
pub use mount::{MountError, detect_project_root, process_host_mounts};
pub use notifying::NotifyingFsLayer;
pub use overlay::OverlayFs;
pub use procfs::{HookLister, ProcFs, ProcState, ToolLister};
pub use readonly_path_guard::ReadOnlyPathGuard;
pub use servicefs::{IntegrationLister, ServiceFs};
pub use simulacra_types::{VfsEvent, VfsWatcher};

#[cfg(test)]
mod service_discovery_vfs;
#[cfg(test)]
mod tests;
