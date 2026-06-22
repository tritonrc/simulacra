use std::collections::HashMap;
use std::sync::Arc;

use crate::ids::{AgentId, ChannelId, MemoryPoolId, SkillId, TenantId};
use crate::models::{Agent, AgentFile, Channel, MemoryPool, Skill, Tenant};

pub mod agent;
pub mod agent_file;
pub mod channel;
pub mod memory_pool;
pub mod skill;
pub mod tenant;

pub use agent::MemoryAgentRepository;
pub use agent_file::MemoryAgentFileRepository;
pub use channel::MemoryChannelRepository;
pub use memory_pool::MemoryMemoryPoolRepository;
pub use skill::MemorySkillRepository;
pub use tenant::MemoryTenantRepository;

#[derive(Debug, Default)]
pub struct InMemoryFixtures {
    pub tenants: HashMap<TenantId, Tenant>,
    pub agents: HashMap<AgentId, Agent>,
    pub agent_skills: HashMap<AgentId, Vec<SkillId>>,
    pub agent_capabilities: HashMap<AgentId, Vec<String>>,
    pub agent_files: HashMap<AgentId, Vec<AgentFile>>,
    pub skills: HashMap<SkillId, Skill>,
    pub memory_pools: HashMap<MemoryPoolId, MemoryPool>,
    pub channels: HashMap<ChannelId, Channel>,
    pub agent_channels: HashMap<AgentId, Vec<ChannelId>>,
}

pub type SharedFixtures = Arc<InMemoryFixtures>;
