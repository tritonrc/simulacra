pub mod agent;
pub mod agent_file;
pub mod channel;
pub mod connection;
pub mod memory_pool;
pub mod scalars;
pub mod skill;
pub mod tool;

use async_graphql::{EmptySubscription, MergedObject, Schema};

pub use tool::{Tool, ToolKind, ToolQuery};

pub type SimulacraSchema = Schema<QueryRoot, MutationRoot, EmptySubscription>;

#[derive(MergedObject, Default)]
pub struct QueryRoot(
    pub agent::AgentQuery,
    pub skill::SkillQuery,
    pub memory_pool::MemoryPoolQuery,
    pub tool::ToolQuery,
    pub agent_file::AgentFileQuery,
    pub channel::ChannelQuery,
);

#[derive(MergedObject, Default)]
pub struct MutationRoot(
    pub agent::AgentMutation,
    pub skill::SkillMutation,
    pub memory_pool::MemoryPoolMutation,
    pub agent_file::AgentFileMutation,
    pub channel::ChannelMutation,
);
