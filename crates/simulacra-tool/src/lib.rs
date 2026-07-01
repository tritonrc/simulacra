//! Simulacra tool crate.
//!
//! Registry for tools that can be offered to an LLM and invoked
//! when the model returns a tool-use response.

mod error;
mod registry;
#[cfg(feature = "sandbox")]
mod sandbox_tools;
mod skills;

pub mod memory;

pub use error::SkillError;
pub use memory::{
    MemoryReadChunkTool, MemoryToolHandles, SemanticSearchTool, register_memory_tools,
};
pub use registry::ToolRegistry;
#[cfg(feature = "sandbox")]
pub use sandbox_tools::register_builtins;
pub use simulacra_types::{CapabilityToken, Tool, ToolDefinition, ToolError};
#[cfg(feature = "sandbox")]
pub use skills::SkillTool;
pub use skills::{SkillMeta, discover_and_filter_skills, parse_skill_frontmatter};
