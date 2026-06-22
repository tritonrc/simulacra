use async_trait::async_trait;

use crate::error::CatalogError;
use crate::ids::{MemoryPoolId, TenantId};
use crate::models::{MemoryPool, MemoryPoolPatch, NewMemoryPool};
use crate::repo::MemoryPoolRepository;
use crate::repo::memory::SharedFixtures;

pub struct MemoryMemoryPoolRepository {
    fixtures: SharedFixtures,
}

impl MemoryMemoryPoolRepository {
    pub fn new(fixtures: SharedFixtures) -> Self {
        Self { fixtures }
    }
}

#[async_trait]
impl MemoryPoolRepository for MemoryMemoryPoolRepository {
    async fn get(
        &self,
        tenant_id: &TenantId,
        id: &MemoryPoolId,
    ) -> Result<MemoryPool, CatalogError> {
        self.fixtures
            .memory_pools
            .get(id)
            .filter(|p| &p.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| {
                CatalogError::NotFound(format!(
                    "memory_pool id={} tenant={}",
                    id.as_str(),
                    tenant_id.as_str()
                ))
            })
    }

    async fn get_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<MemoryPool, CatalogError> {
        self.fixtures
            .memory_pools
            .values()
            .find(|p| &p.tenant_id == tenant_id && p.name == name)
            .cloned()
            .ok_or_else(|| {
                CatalogError::NotFound(format!(
                    "memory_pool name={name} tenant={}",
                    tenant_id.as_str()
                ))
            })
    }

    async fn list(&self, tenant_id: &TenantId) -> Result<Vec<MemoryPool>, CatalogError> {
        let mut out: Vec<MemoryPool> = self
            .fixtures
            .memory_pools
            .values()
            .filter(|p| &p.tenant_id == tenant_id)
            .cloned()
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
        _input: NewMemoryPool<'_>,
    ) -> Result<MemoryPool, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support memory_pool creation".into(),
        ))
    }

    async fn update(
        &self,
        _tenant_id: &TenantId,
        _id: &MemoryPoolId,
        _input: MemoryPoolPatch<'_>,
    ) -> Result<MemoryPool, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support memory_pool update".into(),
        ))
    }

    async fn delete(&self, _tenant_id: &TenantId, _id: &MemoryPoolId) -> Result<(), CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support memory_pool deletion".into(),
        ))
    }
}
