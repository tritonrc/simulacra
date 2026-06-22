use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use simulacra_catalog::CatalogError;
use simulacra_catalog::ids::TenantId;
use simulacra_catalog::repo::TenantRepository;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedPrincipal {
    pub tenant_namespace: String,
    pub subject: String,
}

#[derive(Clone, Debug)]
pub struct GraphQLContext {
    pub tenant_id: TenantId,
    pub principal: AuthenticatedPrincipal,
}

#[derive(Clone)]
pub struct TenantResolver {
    repo: Arc<dyn TenantRepository>,
    cache: Arc<RwLock<HashMap<String, TenantId>>>,
}

impl TenantResolver {
    pub fn new(repo: Arc<dyn TenantRepository>) -> Self {
        Self {
            repo,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn resolve(&self, namespace: &str) -> Result<TenantId, CatalogError> {
        if let Some(id) = self.cache.read().get(namespace) {
            return Ok(id.clone());
        }
        let tenant = self.repo.get_by_namespace(namespace).await?;
        self.cache
            .write()
            .insert(namespace.to_owned(), tenant.id.clone());
        Ok(tenant.id)
    }

    pub fn invalidate(&self, namespace: &str) {
        self.cache.write().remove(namespace);
    }
}
