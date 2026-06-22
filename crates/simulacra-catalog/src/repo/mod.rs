use async_trait::async_trait;

use crate::error::CatalogError;
use crate::ids::{AgentFileId, AgentId, ChannelId, MemoryPoolId, SkillId, TenantId};
use crate::models::*;

pub mod memory;
pub mod sqlite;

#[async_trait]
pub trait TenantRepository: Send + Sync {
    async fn get_by_namespace(&self, namespace: &str) -> Result<Tenant, CatalogError>;
    async fn get_by_id(&self, id: &TenantId) -> Result<Tenant, CatalogError>;
    async fn create(
        &self,
        namespace: &str,
        display_name: Option<&str>,
    ) -> Result<Tenant, CatalogError>;
    async fn get_or_create(
        &self,
        namespace: &str,
        display_name: Option<&str>,
    ) -> Result<Tenant, CatalogError>;
}

#[async_trait]
pub trait MemoryPoolRepository: Send + Sync {
    async fn get(
        &self,
        tenant_id: &TenantId,
        id: &MemoryPoolId,
    ) -> Result<MemoryPool, CatalogError>;
    async fn get_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<MemoryPool, CatalogError>;
    async fn list(&self, tenant_id: &TenantId) -> Result<Vec<MemoryPool>, CatalogError>;
    async fn create(
        &self,
        tenant_id: &TenantId,
        input: NewMemoryPool<'_>,
    ) -> Result<MemoryPool, CatalogError>;
    async fn update(
        &self,
        tenant_id: &TenantId,
        id: &MemoryPoolId,
        input: MemoryPoolPatch<'_>,
    ) -> Result<MemoryPool, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &MemoryPoolId) -> Result<(), CatalogError>;
}

#[async_trait]
pub trait SkillRepository: Send + Sync {
    async fn get(&self, tenant_id: &TenantId, id: &SkillId) -> Result<Skill, CatalogError>;
    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Skill, CatalogError>;
    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Skill>, CatalogError>;
    async fn list_for_agent(
        &self,
        tenant_id: &TenantId,
        agent_id: &AgentId,
    ) -> Result<Vec<Skill>, CatalogError>;
    async fn create(
        &self,
        tenant_id: &TenantId,
        input: NewSkill<'_>,
    ) -> Result<Skill, CatalogError>;
    async fn update(
        &self,
        tenant_id: &TenantId,
        id: &SkillId,
        input: SkillPatch<'_>,
    ) -> Result<Skill, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &SkillId) -> Result<(), CatalogError>;
}

#[async_trait]
pub trait AgentRepository: Send + Sync {
    async fn get(&self, tenant_id: &TenantId, id: &AgentId) -> Result<Agent, CatalogError>;
    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Agent, CatalogError>;
    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Agent>, CatalogError>;
    async fn create(
        &self,
        tenant_id: &TenantId,
        input: NewAgent<'_>,
    ) -> Result<Agent, CatalogError>;
    async fn update(
        &self,
        tenant_id: &TenantId,
        id: &AgentId,
        input: AgentPatch<'_>,
    ) -> Result<Agent, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &AgentId) -> Result<(), CatalogError>;
    async fn capabilities(
        &self,
        tenant_id: &TenantId,
        id: &AgentId,
    ) -> Result<Vec<String>, CatalogError>;
    async fn resolve(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<ResolvedAgent, CatalogError>;
}

/// S046 — Channels are tenant-scoped named entry points an agent can
/// listen on. v1 records the binding; runtime dispatch is S047+.
#[async_trait]
pub trait ChannelRepository: Send + Sync {
    async fn get(&self, tenant_id: &TenantId, id: &ChannelId) -> Result<Channel, CatalogError>;
    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Channel>, CatalogError>;
    async fn list_for_agent(
        &self,
        tenant_id: &TenantId,
        agent_id: &AgentId,
    ) -> Result<Vec<Channel>, CatalogError>;
    async fn create(
        &self,
        tenant_id: &TenantId,
        input: NewChannel<'_>,
    ) -> Result<Channel, CatalogError>;
    async fn update(
        &self,
        tenant_id: &TenantId,
        id: &ChannelId,
        input: ChannelPatch<'_>,
    ) -> Result<Channel, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &ChannelId) -> Result<(), CatalogError>;
}

/// S045 — Per-agent file metadata. Bytes go through the
/// [`crate::AgentFileStore`] seam; this trait owns the catalog rows and
/// uses the store internally.
#[async_trait]
pub trait AgentFileRepository: Send + Sync {
    async fn create(
        &self,
        tenant_id: &TenantId,
        input: NewAgentFile<'_>,
    ) -> Result<AgentFile, CatalogError>;
    async fn get(&self, tenant_id: &TenantId, id: &AgentFileId) -> Result<AgentFile, CatalogError>;
    async fn list_for_agent(
        &self,
        tenant_id: &TenantId,
        agent_id: &AgentId,
    ) -> Result<Vec<AgentFile>, CatalogError>;
    async fn read_bytes(
        &self,
        tenant_id: &TenantId,
        id: &AgentFileId,
    ) -> Result<Vec<u8>, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &AgentFileId) -> Result<(), CatalogError>;
}
