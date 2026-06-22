//! S046 — In-memory `ChannelRepository` for `--no-catalog` mode.
//!
//! Read-only: TOML-defined agents don't carry channel definitions in
//! v1, so all mutating ops return `CatalogError::ReadOnly`. Reads exist
//! for parity with the SQLite repo so callers can iterate channels
//! uniformly across the two backends.

use async_trait::async_trait;

use crate::error::CatalogError;
use crate::ids::{AgentId, ChannelId, TenantId};
use crate::models::{Channel, ChannelPatch, NewChannel, Page, PageRequest};
use crate::repo::ChannelRepository;
use crate::repo::memory::SharedFixtures;
use crate::repo::sqlite::{decode_cursor, encode_cursor};

pub struct MemoryChannelRepository {
    fixtures: SharedFixtures,
}

impl MemoryChannelRepository {
    pub fn new(fixtures: SharedFixtures) -> Self {
        Self { fixtures }
    }
}

#[async_trait]
impl ChannelRepository for MemoryChannelRepository {
    async fn get(&self, tenant_id: &TenantId, id: &ChannelId) -> Result<Channel, CatalogError> {
        self.fixtures
            .channels
            .get(id)
            .filter(|c| &c.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| {
                CatalogError::NotFound(format!(
                    "channel id={} tenant={}",
                    id.as_str(),
                    tenant_id.as_str()
                ))
            })
    }

    async fn list(
        &self,
        tenant_id: &TenantId,
        page: PageRequest,
        name_contains: Option<&str>,
    ) -> Result<Page<Channel>, CatalogError> {
        let mut items: Vec<Channel> = self
            .fixtures
            .channels
            .values()
            .filter(|c| &c.tenant_id == tenant_id)
            .cloned()
            .collect();
        if let Some(needle) = name_contains {
            items.retain(|c| c.name.contains(needle));
        }
        items.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });

        if let Some(cursor) = &page.after {
            let (after_ts, after_id) = decode_cursor(cursor)?;
            items.retain(|c| {
                let ts = c.created_at.to_rfc3339();
                (ts.as_str(), c.id.as_str()) > (after_ts.as_str(), after_id.as_str())
            });
        }

        let limit = page.first.unwrap_or(20).max(1) as usize;
        let has_next_page = items.len() > limit;
        if has_next_page {
            items.truncate(limit);
        }
        let start_cursor = items
            .first()
            .map(|c| encode_cursor(&c.created_at.to_rfc3339(), c.id.as_str()));
        let end_cursor = items
            .last()
            .map(|c| encode_cursor(&c.created_at.to_rfc3339(), c.id.as_str()));

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
    ) -> Result<Vec<Channel>, CatalogError> {
        let agent_in_tenant = self
            .fixtures
            .agents
            .get(agent_id)
            .map(|a| &a.tenant_id == tenant_id)
            .unwrap_or(false);
        if !agent_in_tenant {
            return Ok(Vec::new());
        }
        let ids = self
            .fixtures
            .agent_channels
            .get(agent_id)
            .cloned()
            .unwrap_or_default();
        let mut out: Vec<Channel> = ids
            .iter()
            .filter_map(|cid| self.fixtures.channels.get(cid).cloned())
            .filter(|c| &c.tenant_id == tenant_id)
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
        _input: NewChannel<'_>,
    ) -> Result<Channel, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support channel creation".into(),
        ))
    }

    async fn update(
        &self,
        _tenant_id: &TenantId,
        _id: &ChannelId,
        _input: ChannelPatch<'_>,
    ) -> Result<Channel, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support channel update".into(),
        ))
    }

    async fn delete(&self, _tenant_id: &TenantId, _id: &ChannelId) -> Result<(), CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support channel deletion".into(),
        ))
    }
}
