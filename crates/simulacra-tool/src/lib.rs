//! Simulacra tool crate.
//!
//! Registry for tools that can be offered to an LLM and invoked
//! when the model returns a tool-use response.

mod error;
mod file_presenter;
mod registry;
#[cfg(feature = "sandbox")]
mod sandbox_tools;
mod skills;

pub mod memory;

pub use error::SkillError;
pub use file_presenter::{
    FilePresenter, FileReadRequest, LossyTextPresenter, PresentFuture, byte_window,
};
pub use memory::{
    MemoryReadChunkTool, MemoryToolHandles, SemanticSearchTool, register_memory_tools,
};
pub use registry::{ToolExposure, ToolMetadata, ToolRegistry};
#[cfg(feature = "sandbox")]
pub use sandbox_tools::{
    file_tools, register_builtins, register_builtins_with_presenter, register_exec_tools,
    register_file_tools, register_file_tools_with_presenter,
};
pub use simulacra_types::{
    CapabilityToken, Tool, ToolDefinition, ToolError, ToolOutput, ToolSchema,
};
#[cfg(feature = "sandbox")]
pub use skills::SkillTool;
pub use skills::{
    SkillMeta, discover_and_filter_skills, discover_and_filter_skills_for_placement,
    parse_skill_frontmatter,
};
