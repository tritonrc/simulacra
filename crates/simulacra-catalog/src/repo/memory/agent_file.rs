//! S045 — In-memory `AgentFileRepository` for `--no-catalog` mode.
//!
//! Read-only by construction: TOML-defined agents have no per-agent
//! file inputs, so create/delete return `CatalogError::ReadOnly`. The
//! `list_for_agent` / `get` / `read_bytes` paths exist for parity with
//! the SQLite repo so callers can iterate files uniformly.

use async_trait::async_trait;

use crate::error::CatalogError;
use crate::ids::{AgentFileId, AgentId, TenantId};
use crate::models::{AgentFile, NewAgentFile};
use crate::repo::AgentFileRepository;
use crate::repo::memory::SharedFixtures;

pub struct MemoryAgentFileRepository {
    fixtures: SharedFixtures,
}

impl MemoryAgentFileRepository {
    pub fn new(fixtures: SharedFixtures) -> Self {
        Self { fixtures }
    }

    fn agent_in_tenant(&self, tenant_id: &TenantId, agent_id: &AgentId) -> bool {
        self.fixtures
            .agents
            .get(agent_id)
            .map(|a| &a.tenant_id == tenant_id)
            .unwrap_or(false)
    }
}

#[async_trait]
impl AgentFileRepository for MemoryAgentFileRepository {
    async fn create(
        &self,
        _tenant_id: &TenantId,
        _input: NewAgentFile<'_>,
    ) -> Result<AgentFile, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support agent_file creation".into(),
        ))
    }

    async fn get(&self, tenant_id: &TenantId, id: &AgentFileId) -> Result<AgentFile, CatalogError> {
        for files in self.fixtures.agent_files.values() {
            for f in files {
                if &f.id == id && self.agent_in_tenant(tenant_id, &f.agent_id) {
                    return Ok(f.clone());
                }
            }
        }
        Err(CatalogError::NotFound(format!(
            "agent_file id={} tenant={}",
            id.as_str(),
            tenant_id.as_str()
        )))
    }

    async fn list_for_agent(
        &self,
        tenant_id: &TenantId,
        agent_id: &AgentId,
    ) -> Result<Vec<AgentFile>, CatalogError> {
        if !self.agent_in_tenant(tenant_id, agent_id) {
            return Ok(Vec::new());
        }
        let mut files = self
            .fixtures
            .agent_files
            .get(agent_id)
            .cloned()
            .unwrap_or_default();
        files.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(files)
    }

    async fn read_bytes(
        &self,
        _tenant_id: &TenantId,
        _id: &AgentFileId,
    ) -> Result<Vec<u8>, CatalogError> {
        // Memory fixtures don't carry blob bodies — they only seed
        // metadata. Return ReadOnly to surface the limitation; tests
        // that need byte round-trips must use the SQLite repo.
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not store agent_file bytes".into(),
        ))
    }

    async fn delete(&self, _tenant_id: &TenantId, _id: &AgentFileId) -> Result<(), CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support agent_file deletion".into(),
        ))
    }
}
