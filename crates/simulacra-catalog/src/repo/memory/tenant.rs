use async_trait::async_trait;

use crate::error::CatalogError;
use crate::ids::TenantId;
use crate::models::Tenant;
use crate::repo::TenantRepository;
use crate::repo::memory::SharedFixtures;

pub struct MemoryTenantRepository {
    fixtures: SharedFixtures,
}

impl MemoryTenantRepository {
    pub fn new(fixtures: SharedFixtures) -> Self {
        Self { fixtures }
    }
}

#[async_trait]
impl TenantRepository for MemoryTenantRepository {
    async fn get_by_namespace(&self, namespace: &str) -> Result<Tenant, CatalogError> {
        self.fixtures
            .tenants
            .values()
            .find(|t| t.namespace == namespace)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound(format!("tenant ns={namespace}")))
    }

    async fn get_by_id(&self, id: &TenantId) -> Result<Tenant, CatalogError> {
        self.fixtures
            .tenants
            .get(id)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound(format!("tenant id={}", id.as_str())))
    }

    async fn create(
        &self,
        _namespace: &str,
        _display_name: Option<&str>,
    ) -> Result<Tenant, CatalogError> {
        Err(CatalogError::ReadOnly(
            "--no-catalog mode does not support tenant creation".into(),
        ))
    }

    async fn get_or_create(
        &self,
        namespace: &str,
        _display_name: Option<&str>,
    ) -> Result<Tenant, CatalogError> {
        // The SQLite implementation creates on miss; in `--no-catalog` mode the
        // fixture set is immutable, so a miss must surface as ReadOnly to keep
        // both impls of the trait method behaviorally aligned (callers cannot
        // distinguish backends).
        match self.get_by_namespace(namespace).await {
            Ok(t) => Ok(t),
            Err(CatalogError::NotFound(_)) => Err(CatalogError::ReadOnly(
                "--no-catalog mode does not create tenants".into(),
            )),
            Err(e) => Err(e),
        }
    }
}
