use async_trait::async_trait;

use crate::error::CatalogError;
use crate::ids::{AgentId, TenantId};
use crate::models::{
    Agent, AgentFile, AgentPatch, Channel, NewAgent, Page, PageRequest, ResolvedAgent, Skill,
};
use crate::repo::AgentRepository;
use crate::repo::memory::SharedFixtures;
use crate::repo::sqlite::{decode_cursor, encode_cursor};

pub struct MemoryAgentRepository {
    fixtures: SharedFixtures,
}

impl MemoryAgentRepository {
    pub fn new(fixtures: SharedFixtures) -> Self {
        Self { fixtures }
    }
}

#[async_trait]
impl AgentRepository for MemoryAgentRepository {
    async fn get(&self, tenant_id: &TenantId, id: &AgentId) -> Result<Agent, CatalogError> {
        self.fixtures
            .agents
            .get(id)
            .filter(|a| &a.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| {
                CatalogError::NotFound(format!(
                    "agent id={} tenant={}",
                    id.as_str(),
                    tenant_id.as_str()
                ))
            })
    }

    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Agent, CatalogError> {
        self.fixtures
            .agents
            .values()
            .find(|a| &a.tenant_id == tenant_id && a.name == name)
            .cloned()
            .ok_or_else(|| {
                CatalogError::NotFound(format!("agent name={name} tenant={}", tenant_id.as_str()))
            })
    }

    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Agent>, CatalogError> {
        let mut items: Vec<Agent> = self
            .fixtures
            .agents
            .values()
            .filter(|a| &a.tenant_id == tenant_id)
            .cloned()
            .collect();
        // Apply the name-contains filter before sort/pagination so cursor
        // positions reflect the filtered universe (mirrors SQL `AND name LIKE`).
        if let Some(needle) = name_contains {
            items.retain(|a| a.name.contains(needle));
        }
        items.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });

        // Skip rows up to and including the cursor (created_at, id) tuple.
        if let Some(cursor) = &page.after {
            let (after_ts, after_id) = decode_cursor(cursor)?;
            items.retain(|a| {
                let ts = a.created_at.to_rfc3339();
                (ts.as_str(), a.id.as_str()) > (after_ts.as_str(), after_id.as_str())
            });
        }

        let limit = page.first.unwrap_or(20).max(1) as usize;
        let has_next_page = items.len() > limit;
        if has_next_page {
            items.truncate(limit);
        }
        let start_cursor = items
            .first()
            .map(|a| encode_cursor(&a.created_at.to_rfc3339(), a.id.as_str()));
        let end_cursor = items
            .last()
            .map(|a| encode_cursor(&a.created_at.to_rfc3339(), a.id.as_str()));

        Ok(Page {
            items,
            end_cursor,
            start_cursor,
            has_next_page,
            has_previous_page: false,
        })
    }

    async fn create(
        &self,
        _tenant_id: &TenantId,
        _input: NewAgent<'_>,
    ) -> Result<Agent, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support agent creation".into(),
        ))
    }

    async fn update(
        &self,
        _tenant_id: &TenantId,
        _id: &AgentId,
        _input: AgentPatch<'_>,
    ) -> Result<Agent, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support agent update".into(),
        ))
    }

    async fn delete(&self, _tenant_id: &TenantId, _id: &AgentId) -> Result<(), CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support agent deletion".into(),
        ))
    }

    async fn capabilities(
        &self,
        tenant_id: &TenantId,
        id: &AgentId,
    ) -> Result<Vec<String>, CatalogError> {
        let visible = self
            .fixtures
            .agents
            .get(id)
            .map(|agent| &agent.tenant_id == tenant_id)
            .unwrap_or(false);
        if !visible {
            return Ok(Vec::new());
        }

        Ok(self
            .fixtures
            .agent_capabilities
            .get(id)
            .cloned()
            .unwrap_or_default())
    }

    async fn resolve(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<ResolvedAgent, CatalogError> {
        let agent = self.get_by_name(tenant_id, name).await?;
        let skill_ids = self
            .fixtures
            .agent_skills
            .get(&agent.id)
            .cloned()
            .unwrap_or_default();
        // Filter joined skills to the resolve request's tenant. A miswired
        // fixture must not surface foreign-tenant rows just because an
        // `agent_skills` row references them.
        let skills: Vec<Skill> = skill_ids
            .iter()
            .filter_map(|sid| self.fixtures.skills.get(sid).cloned())
            .filter(|s| &s.tenant_id == tenant_id)
            .collect();
        let capabilities = self
            .fixtures
            .agent_capabilities
            .get(&agent.id)
            .cloned()
            .unwrap_or_default();
        // Same for memory_pool: only join across rows that belong to the
        // requested tenant.
        let memory_pool = agent
            .memory_pool_id
            .as_ref()
            .and_then(|id| self.fixtures.memory_pools.get(id).cloned())
            .filter(|p| &p.tenant_id == tenant_id);
        // S045: per-agent files. The fixtures map keys files by AgentId;
        // mirror the same tenant filter applied to skills/memory_pool to
        // prevent foreign-tenant rows from leaking via a miswired fixture.
        let files: Vec<AgentFile> = self
            .fixtures
            .agent_files
            .get(&agent.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f.agent_id == agent.id)
            .collect();
        // S046: channels. Lookup via the agent_channels join, then filter
        // to the requested tenant for the same fixture-safety reason as
        // skills.
        let channel_ids = self
            .fixtures
            .agent_channels
            .get(&agent.id)
            .cloned()
            .unwrap_or_default();
        let channels: Vec<Channel> = channel_ids
            .iter()
            .filter_map(|cid| self.fixtures.channels.get(cid).cloned())
            .filter(|c| &c.tenant_id == tenant_id)
            .collect();
        Ok(ResolvedAgent {
            id: agent.id,
            name: agent.name,
            system_prompt: agent.system_prompt,
            model: agent.model,
            max_turns: agent.max_turns,
            max_tokens: agent.max_tokens,
            skills,
            capabilities,
            memory_pool,
            files,
            channels,
        })
    }
}
