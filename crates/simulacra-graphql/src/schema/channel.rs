//! S046 — GraphQL surface for channels.

use std::sync::Arc;

use async_graphql::{Context, Enum, ID, InputObject, MaybeUndefined, Object, SimpleObject};
use simulacra_catalog::CatalogError;
use simulacra_catalog::ids::ChannelId;
use simulacra_catalog::models::{
    Channel as ChannelModel, ChannelKind as ChannelKindModel, ChannelPatch, NewChannel, PageRequest,
};
use simulacra_catalog::repo::ChannelRepository;

use crate::context::GraphQLContext;
use crate::error::to_field_error;
use crate::schema::agent::page_request_from;
use crate::schema::connection::{PageInfoExt, PageInput, encode_cursor};
use crate::schema::scalars::{DateTimeScalar, JsonScalar};

#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
#[graphql(name = "ChannelKind")]
pub enum ChannelKind {
    Slack,
    Teams,
    Email,
    Webhook,
    Manual,
}

impl From<ChannelKindModel> for ChannelKind {
    fn from(k: ChannelKindModel) -> Self {
        match k {
            ChannelKindModel::Slack => ChannelKind::Slack,
            ChannelKindModel::Teams => ChannelKind::Teams,
            ChannelKindModel::Email => ChannelKind::Email,
            ChannelKindModel::Webhook => ChannelKind::Webhook,
            ChannelKindModel::Manual => ChannelKind::Manual,
        }
    }
}

impl From<ChannelKind> for ChannelKindModel {
    fn from(k: ChannelKind) -> Self {
        match k {
            ChannelKind::Slack => ChannelKindModel::Slack,
            ChannelKind::Teams => ChannelKindModel::Teams,
            ChannelKind::Email => ChannelKindModel::Email,
            ChannelKind::Webhook => ChannelKindModel::Webhook,
            ChannelKind::Manual => ChannelKindModel::Manual,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChannelNode(pub ChannelModel);

#[Object(name = "Channel")]
impl ChannelNode {
    async fn id(&self) -> ID {
        ID(self.0.id.0.clone())
    }

    async fn tenant_id(&self) -> ID {
        ID(self.0.tenant_id.0.clone())
    }

    async fn name(&self) -> &str {
        &self.0.name
    }

    async fn kind(&self) -> ChannelKind {
        self.0.kind.into()
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

#[derive(SimpleObject, Clone, Debug)]
pub struct ChannelEdge {
    pub node: ChannelNode,
    pub cursor: String,
}

#[derive(SimpleObject, Clone, Debug)]
pub struct ChannelConnection {
    pub edges: Vec<ChannelEdge>,
    #[graphql(name = "pageInfo")]
    pub page_info: PageInfoExt,
}

#[derive(InputObject, Clone, Debug, Default)]
pub struct ChannelFilter {
    pub name_contains: Option<String>,
}

#[derive(InputObject, Clone, Debug)]
pub struct CreateChannelInput {
    pub name: String,
    pub kind: ChannelKind,
    pub config: Option<JsonScalar>,
}

#[derive(InputObject, Clone, Debug, Default)]
pub struct UpdateChannelInput {
    pub name: Option<String>,
    pub kind: Option<ChannelKind>,
    pub config: MaybeUndefined<JsonScalar>,
}

#[derive(Default)]
pub struct ChannelQuery;

#[Object]
impl ChannelQuery {
    async fn channel(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> async_graphql::Result<Option<ChannelNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let channels = ctx.data::<Arc<dyn ChannelRepository>>()?;
        let cid = ChannelId(id.to_string());
        match channels.get(&gql.tenant_id, &cid).await {
            Ok(c) => Ok(Some(ChannelNode(c))),
            Err(CatalogError::NotFound(_)) => Ok(None),
            Err(e) => Err(to_field_error(e)),
        }
    }

    async fn channels(
        &self,
        ctx: &Context<'_>,
        filter: Option<ChannelFilter>,
        page: Option<PageInput>,
    ) -> async_graphql::Result<ChannelConnection> {
        let gql = ctx.data::<GraphQLContext>()?;
        let channels = ctx.data::<Arc<dyn ChannelRepository>>()?;
        let req: PageRequest = page_request_from(page);
        let needle: Option<String> = filter
            .and_then(|f| f.name_contains)
            .filter(|s| !s.is_empty());
        let result = channels
            .list(&gql.tenant_id, req, needle.as_deref())
            .await
            .map_err(to_field_error)?;

        let edges: Vec<ChannelEdge> = result
            .items
            .into_iter()
            .map(|c| {
                let cursor = encode_cursor(c.created_at, c.id.as_str());
                ChannelEdge {
                    node: ChannelNode(c),
                    cursor,
                }
            })
            .collect();

        let page_info = PageInfoExt {
            has_next_page: result.has_next_page,
            has_previous_page: result.has_previous_page,
            start_cursor: edges
                .first()
                .map(|e| e.cursor.clone())
                .or(result.start_cursor),
            end_cursor: edges.last().map(|e| e.cursor.clone()).or(result.end_cursor),
        };

        Ok(ChannelConnection { edges, page_info })
    }
}

#[derive(Default)]
pub struct ChannelMutation;

#[Object]
impl ChannelMutation {
    async fn create_channel(
        &self,
        ctx: &Context<'_>,
        input: CreateChannelInput,
    ) -> async_graphql::Result<ChannelNode> {
        let gql = ctx.data::<GraphQLContext>()?;
        let channels = ctx.data::<Arc<dyn ChannelRepository>>()?;
        let config = input.config.as_ref().map(|c| c.0.clone());
        let new = NewChannel {
            name: &input.name,
            kind: input.kind.into(),
            config: config.as_ref(),
        };
        let row = channels
            .create(&gql.tenant_id, new)
            .await
            .map_err(to_field_error)?;
        Ok(ChannelNode(row))
    }

    async fn update_channel(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: UpdateChannelInput,
    ) -> async_graphql::Result<ChannelNode> {
        let gql = ctx.data::<GraphQLContext>()?;
        let channels = ctx.data::<Arc<dyn ChannelRepository>>()?;
        let cid = ChannelId(id.to_string());

        let kind = input.kind.map(|k| k.into());
        let config: Option<Option<serde_json::Value>> = match input.config {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(None),
            MaybeUndefined::Value(v) => Some(Some(v.0)),
        };

        let patch = ChannelPatch {
            name: input.name.as_deref(),
            kind,
            config: config.as_ref().map(|inner| inner.as_ref()),
        };
        let row = channels
            .update(&gql.tenant_id, &cid, patch)
            .await
            .map_err(to_field_error)?;
        Ok(ChannelNode(row))
    }

    async fn delete_channel(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<bool> {
        let gql = ctx.data::<GraphQLContext>()?;
        let channels = ctx.data::<Arc<dyn ChannelRepository>>()?;
        let cid = ChannelId(id.to_string());
        match channels.delete(&gql.tenant_id, &cid).await {
            Ok(()) => Ok(true),
            Err(CatalogError::NotFound(_)) => Ok(false),
            Err(e) => Err(to_field_error(e)),
        }
    }
}
