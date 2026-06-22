use std::sync::Arc;

use async_graphql::{Context, ID, InputObject, MaybeUndefined, Object, SimpleObject};
use simulacra_catalog::CatalogError;
use simulacra_catalog::ids::SkillId;
use simulacra_catalog::models::{NewSkill, Skill as SkillModel, SkillPatch};
use simulacra_catalog::repo::SkillRepository;

use crate::context::GraphQLContext;
use crate::error::to_field_error;
use crate::schema::agent::page_request_from;
use crate::schema::connection::{PageInfoExt, PageInput, encode_cursor};
use crate::schema::scalars::{DateTimeScalar, JsonScalar};

#[derive(Clone, Debug)]
pub struct SkillNode(pub SkillModel);

#[Object(name = "Skill")]
impl SkillNode {
    async fn id(&self) -> ID {
        ID(self.0.id.0.clone())
    }

    async fn name(&self) -> &str {
        &self.0.name
    }

    async fn description(&self) -> Option<&str> {
        self.0.description.as_deref()
    }

    async fn body(&self) -> &str {
        &self.0.body
    }

    async fn metadata(&self) -> Option<JsonScalar> {
        self.0.metadata.clone().map(async_graphql::Json)
    }

    async fn created_at(&self) -> DateTimeScalar {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTimeScalar {
        self.0.updated_at
    }
}

#[derive(SimpleObject, Clone, Debug)]
pub struct SkillEdge {
    pub node: SkillNode,
    pub cursor: String,
}

#[derive(SimpleObject, Clone, Debug)]
pub struct SkillConnection {
    pub edges: Vec<SkillEdge>,
    #[graphql(name = "pageInfo")]
    pub page_info: PageInfoExt,
}

#[derive(InputObject, Clone, Debug, Default)]
pub struct SkillFilter {
    pub name_contains: Option<String>,
}

#[derive(InputObject, Clone, Debug)]
pub struct CreateSkillInput {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    pub metadata: Option<JsonScalar>,
}

#[derive(InputObject, Clone, Debug, Default)]
pub struct UpdateSkillInput {
    pub name: Option<String>,
    pub description: MaybeUndefined<String>,
    pub body: Option<String>,
    pub metadata: MaybeUndefined<JsonScalar>,
}

#[derive(Default)]
pub struct SkillQuery;

#[Object]
impl SkillQuery {
    async fn skill(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<Option<SkillNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let skills = ctx.data::<Arc<dyn SkillRepository>>()?;
        let sid = SkillId(id.to_string());
        match skills.get(&gql.tenant_id, &sid).await {
            Ok(s) => Ok(Some(SkillNode(s))),
            Err(CatalogError::NotFound(_)) => Ok(None),
            Err(e) => Err(to_field_error(e)),
        }
    }

    async fn skills(
        &self,
        ctx: &Context<'_>,
        filter: Option<SkillFilter>,
        page: Option<PageInput>,
    ) -> async_graphql::Result<SkillConnection> {
        let gql = ctx.data::<GraphQLContext>()?;
        let skills = ctx.data::<Arc<dyn SkillRepository>>()?;
        let req = page_request_from(page);
        // Push the `nameContains` filter down into the repo so pageInfo
        // reflects the filtered universe. Treat empty needles as "no filter".
        let needle: Option<String> = filter
            .and_then(|f| f.name_contains)
            .filter(|s| !s.is_empty());
        let result = skills
            .list(&gql.tenant_id, req, needle.as_deref())
            .await
            .map_err(to_field_error)?;

        let edges: Vec<SkillEdge> = result
            .items
            .into_iter()
            .map(|s: SkillModel| {
                let cursor = encode_cursor(s.created_at, s.id.as_str());
                SkillEdge {
                    node: SkillNode(s),
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

        Ok(SkillConnection { edges, page_info })
    }
}

#[derive(Default)]
pub struct SkillMutation;

#[Object]
impl SkillMutation {
    async fn create_skill(
        &self,
        ctx: &Context<'_>,
        input: CreateSkillInput,
    ) -> async_graphql::Result<SkillNode> {
        let gql = ctx.data::<GraphQLContext>()?;
        let skills = ctx.data::<Arc<dyn SkillRepository>>()?;
        let metadata_value = input.metadata.as_ref().map(|j| j.0.clone());
        let new_skill = NewSkill {
            name: &input.name,
            description: input.description.as_deref(),
            body: &input.body,
            metadata: metadata_value.as_ref(),
        };
        let skill = skills
            .create(&gql.tenant_id, new_skill)
            .await
            .map_err(to_field_error)?;
        Ok(SkillNode(skill))
    }

    async fn update_skill(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: UpdateSkillInput,
    ) -> async_graphql::Result<SkillNode> {
        let gql = ctx.data::<GraphQLContext>()?;
        let skills = ctx.data::<Arc<dyn SkillRepository>>()?;
        let sid = SkillId(id.to_string());

        let description: Option<Option<String>> = match input.description {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(None),
            MaybeUndefined::Value(v) => Some(Some(v)),
        };
        let metadata: Option<Option<serde_json::Value>> = match input.metadata {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(None),
            MaybeUndefined::Value(v) => Some(Some(v.0)),
        };

        let patch = SkillPatch {
            name: input.name.as_deref(),
            description: description.as_ref().map(|inner| inner.as_deref()),
            body: input.body.as_deref(),
            metadata: metadata.as_ref().map(|inner| inner.as_ref()),
        };

        let skill = skills
            .update(&gql.tenant_id, &sid, patch)
            .await
            .map_err(to_field_error)?;
        Ok(SkillNode(skill))
    }

    async fn delete_skill(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<bool> {
        let gql = ctx.data::<GraphQLContext>()?;
        let skills = ctx.data::<Arc<dyn SkillRepository>>()?;
        let sid = SkillId(id.to_string());
        skills
            .delete(&gql.tenant_id, &sid)
            .await
            .map_err(to_field_error)?;
        Ok(true)
    }
}
