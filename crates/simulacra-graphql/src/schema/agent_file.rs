//! S045 — GraphQL surface for per-agent files.

use std::sync::Arc;

use async_graphql::{Context, ID, Object};
use simulacra_catalog::CatalogError;
use simulacra_catalog::ids::AgentFileId;
use simulacra_catalog::models::AgentFile as AgentFileModel;
use simulacra_catalog::repo::AgentFileRepository;

use crate::context::GraphQLContext;
use crate::error::to_field_error;
use crate::schema::scalars::DateTimeScalar;

#[derive(Clone, Debug)]
pub struct AgentFileNode(pub AgentFileModel);

#[Object(name = "AgentFile")]
impl AgentFileNode {
    async fn id(&self) -> ID {
        ID(self.0.id.0.clone())
    }

    async fn agent_id(&self) -> ID {
        ID(self.0.agent_id.0.clone())
    }

    async fn name(&self) -> &str {
        &self.0.name
    }

    async fn mime_type(&self) -> &str {
        &self.0.mime_type
    }

    async fn size_bytes(&self) -> i64 {
        self.0.size_bytes as i64
    }

    /// REST path the UI hits to download bytes. Relative — clients attach
    /// the same auth they use for GraphQL. Format mirrors the upload route
    /// path defined in the S045 spec REST section.
    async fn download_url(&self) -> String {
        format!(
            "/api/v1/agents/{}/files/{}/bytes",
            self.0.agent_id.0, self.0.id.0
        )
    }

    async fn created_at(&self) -> DateTimeScalar {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTimeScalar {
        self.0.updated_at
    }
}

#[derive(Default)]
pub struct AgentFileQuery;

#[Object]
impl AgentFileQuery {
    /// Returns null for unknown id AND for cross-tenant id — no error, no
    /// existence leak.
    async fn agent_file(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> async_graphql::Result<Option<AgentFileNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let files = ctx.data::<Arc<dyn AgentFileRepository>>()?;
        let fid = AgentFileId(id.to_string());
        match files.get(&gql.tenant_id, &fid).await {
            Ok(f) => Ok(Some(AgentFileNode(f))),
            Err(CatalogError::NotFound(_)) => Ok(None),
            Err(e) => Err(to_field_error(e)),
        }
    }
}

#[derive(Default)]
pub struct AgentFileMutation;

#[Object]
impl AgentFileMutation {
    /// `true` if it was deleted; `false` for unknown id OR cross-tenant id.
    /// Cross-tenant returns `false` — not an error — to avoid leaking
    /// existence (S045 assertion: detachAgentFile of cross-tenant id returns
    /// false, does NOT error, does NOT leak existence).
    async fn detach_agent_file(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<bool> {
        let gql = ctx.data::<GraphQLContext>()?;
        let files = ctx.data::<Arc<dyn AgentFileRepository>>()?;
        let fid = AgentFileId(id.to_string());
        match files.delete(&gql.tenant_id, &fid).await {
            Ok(()) => Ok(true),
            Err(CatalogError::NotFound(_)) => Ok(false),
            Err(e) => Err(to_field_error(e)),
        }
    }
}
