use std::sync::Arc;

use async_graphql::{Context, ID, InputObject, MaybeUndefined, Object};
use simulacra_catalog::CatalogError;
use simulacra_catalog::ids::MemoryPoolId;
use simulacra_catalog::models::{MemoryPool as MemoryPoolModel, MemoryPoolPatch, NewMemoryPool};
use simulacra_catalog::repo::MemoryPoolRepository;

use crate::context::GraphQLContext;
use crate::error::to_field_error;
use crate::schema::scalars::{DateTimeScalar, JsonScalar};

#[derive(Clone, Debug)]
pub struct MemoryPoolNode(pub MemoryPoolModel);

#[Object(name = "MemoryPool")]
impl MemoryPoolNode {
    async fn id(&self) -> ID {
        ID(self.0.id.0.clone())
    }

    async fn name(&self) -> &str {
        &self.0.name
    }

    async fn embedding_model(&self) -> Option<&str> {
        self.0.embedding_model.as_deref()
    }

    async fn config(&self) -> JsonScalar {
        async_graphql::Json(self.0.config.clone())
    }

    async fn created_at(&self) -> DateTimeScalar {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTimeScalar {
        self.0.updated_at
    }
}

#[derive(InputObject, Clone, Debug)]
pub struct CreateMemoryPoolInput {
    pub name: String,
    pub embedding_model: Option<String>,
    pub config: JsonScalar,
}

#[derive(InputObject, Clone, Debug, Default)]
pub struct UpdateMemoryPoolInput {
    pub name: Option<String>,
    pub embedding_model: MaybeUndefined<String>,
    pub config: Option<JsonScalar>,
}

#[derive(Default)]
pub struct MemoryPoolQuery;

#[Object]
impl MemoryPoolQuery {
    async fn memory_pool(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> async_graphql::Result<Option<MemoryPoolNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let pools = ctx.data::<Arc<dyn MemoryPoolRepository>>()?;
        let mpid = MemoryPoolId(id.to_string());
        match pools.get(&gql.tenant_id, &mpid).await {
            Ok(p) => Ok(Some(MemoryPoolNode(p))),
            Err(CatalogError::NotFound(_)) => Ok(None),
            Err(e) => Err(to_field_error(e)),
        }
    }

    async fn memory_pools(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<MemoryPoolNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let pools = ctx.data::<Arc<dyn MemoryPoolRepository>>()?;
        let rows = pools.list(&gql.tenant_id).await.map_err(to_field_error)?;
        Ok(rows.into_iter().map(MemoryPoolNode).collect())
    }
}

#[derive(Default)]
pub struct MemoryPoolMutation;

#[Object]
impl MemoryPoolMutation {
    async fn create_memory_pool(
        &self,
        ctx: &Context<'_>,
        input: CreateMemoryPoolInput,
    ) -> async_graphql::Result<MemoryPoolNode> {
        let gql = ctx.data::<GraphQLContext>()?;
        let pools = ctx.data::<Arc<dyn MemoryPoolRepository>>()?;
        let config = input.config.0.clone();
        let new_pool = NewMemoryPool {
            name: &input.name,
            embedding_model: input.embedding_model.as_deref(),
            config: &config,
        };
        let pool = pools
            .create(&gql.tenant_id, new_pool)
            .await
            .map_err(to_field_error)?;
        Ok(MemoryPoolNode(pool))
    }

    async fn update_memory_pool(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: UpdateMemoryPoolInput,
    ) -> async_graphql::Result<MemoryPoolNode> {
        let gql = ctx.data::<GraphQLContext>()?;
        let pools = ctx.data::<Arc<dyn MemoryPoolRepository>>()?;
        let mpid = MemoryPoolId(id.to_string());

        let embedding_model: Option<Option<String>> = match input.embedding_model {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(None),
            MaybeUndefined::Value(v) => Some(Some(v)),
        };
        let config_value = input.config.as_ref().map(|j| j.0.clone());

        let patch = MemoryPoolPatch {
            name: input.name.as_deref(),
            embedding_model: embedding_model.as_ref().map(|inner| inner.as_deref()),
            config: config_value.as_ref(),
        };

        let pool = pools
            .update(&gql.tenant_id, &mpid, patch)
            .await
            .map_err(to_field_error)?;
        Ok(MemoryPoolNode(pool))
    }

    async fn delete_memory_pool(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<bool> {
        let gql = ctx.data::<GraphQLContext>()?;
        let pools = ctx.data::<Arc<dyn MemoryPoolRepository>>()?;
        let mpid = MemoryPoolId(id.to_string());
        pools
            .delete(&gql.tenant_id, &mpid)
            .await
            .map_err(to_field_error)?;
        Ok(true)
    }
}
