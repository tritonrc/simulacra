//! Core types and traits for the Simulacra agent framework.
//! This is the leaf crate — zero internal dependencies.

mod activity;
mod artifact;
mod budget;
mod capability;
mod context;
pub mod evaluation_deadline;
mod journal;
mod memory;
mod message;
mod path_glob;
mod provider;
mod tool;
mod vfs;
mod vfs_ops;

pub use activity::*;
pub use artifact::*;
pub use budget::*;
pub use capability::*;
pub use context::*;
pub use journal::*;
pub use memory::*;
pub use message::*;
pub use path_glob::is_supported as path_pattern_is_supported;
pub use provider::*;
pub use tool::*;
pub use vfs::*;
pub use vfs_ops::{DirListing, FsMutation, Precondition};
