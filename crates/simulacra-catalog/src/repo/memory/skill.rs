use async_trait::async_trait;

use crate::error::CatalogError;
use crate::ids::{AgentId, SkillId, TenantId};
use crate::models::{NewSkill, Page, PageRequest, Skill, SkillPatch};
use crate::repo::SkillRepository;
use crate::repo::memory::SharedFixtures;
use crate::repo::sqlite::{decode_cursor, encode_cursor};

pub struct MemorySkillRepository {
    fixtures: SharedFixtures,
}

impl MemorySkillRepository {
    pub fn new(fixtures: SharedFixtures) -> Self {
        Self { fixtures }
    }
}

#[async_trait]
impl SkillRepository for MemorySkillRepository {
    async fn get(&self, tenant_id: &TenantId, id: &SkillId) -> Result<Skill, CatalogError> {
        self.fixtures
            .skills
            .get(id)
            .filter(|s| &s.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| {
                CatalogError::NotFound(format!(
                    "skill id={} tenant={}",
                    id.as_str(),
                    tenant_id.as_str()
                ))
            })
    }

    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Skill, CatalogError> {
        self.fixtures
            .skills
            .values()
            .find(|s| &s.tenant_id == tenant_id && s.name == name)
            .cloned()
            .ok_or_else(|| {
                CatalogError::NotFound(format!("skill name={name} tenant={}", tenant_id.as_str()))
            })
    }

    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Skill>, CatalogError> {
        let mut items: Vec<Skill> = self
            .fixtures
            .skills
            .values()
            .filter(|s| &s.tenant_id == tenant_id)
            .cloned()
            .collect();
        // Apply the name-contains filter before sort/pagination so cursor
        // positions reflect the filtered universe (mirrors SQL `AND name LIKE`).
        if let Some(needle) = name_contains {
            items.retain(|s| s.name.contains(needle));
        }
        items.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });

        if let Some(cursor) = &page.after {
            let (after_ts, after_id) = decode_cursor(cursor)?;
            items.retain(|s| {
                let ts = s.created_at.to_rfc3339();
                (ts.as_str(), s.id.as_str()) > (after_ts.as_str(), after_id.as_str())
            });
        }

        let limit = page.first.unwrap_or(20).max(1) as usize;
        let has_next_page = items.len() > limit;
        if has_next_page {
            items.truncate(limit);
        }
        let start_cursor = items
            .first()
            .map(|s| encode_cursor(&s.created_at.to_rfc3339(), s.id.as_str()));
        let end_cursor = items
            .last()
            .map(|s| encode_cursor(&s.created_at.to_rfc3339(), s.id.as_str()));

        Ok(Page {
            items,
            end_cursor,
            start_cursor,
            has_next_page,
            has_previous_page: false,
        })
    }

    async fn list_for_agent(
        &self,
        tenant_id: &TenantId,
        agent_id: &AgentId,
    ) -> Result<Vec<Skill>, CatalogError> {
        // Verify the agent belongs to the tenant; otherwise return empty.
        let agent_in_tenant = self
            .fixtures
            .agents
            .get(agent_id)
            .map(|a| &a.tenant_id == tenant_id)
            .unwrap_or(false);
        if !agent_in_tenant {
            return Ok(Vec::new());
        }
        let skill_ids = self
            .fixtures
            .agent_skills
            .get(agent_id)
            .cloned()
            .unwrap_or_default();
        let mut out: Vec<Skill> = skill_ids
            .iter()
            .filter_map(|sid| self.fixtures.skills.get(sid).cloned())
            .filter(|s| &s.tenant_id == tenant_id)
            .collect();
        out.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(out)
    }

    async fn create(
        &self,
        _tenant_id: &TenantId,
        _input: NewSkill<'_>,
    ) -> Result<Skill, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support skill creation".into(),
        ))
    }

    async fn update(
        &self,
        _tenant_id: &TenantId,
        _id: &SkillId,
        _input: SkillPatch<'_>,
    ) -> Result<Skill, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support skill update".into(),
        ))
    }

    async fn delete(&self, _tenant_id: &TenantId, _id: &SkillId) -> Result<(), CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support skill deletion".into(),
        ))
    }
}
