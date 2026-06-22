use std::sync::Arc;

use async_graphql::{Context, ID, InputObject, MaybeUndefined, Object, SimpleObject};
use simulacra_catalog::CatalogError;
use simulacra_catalog::ids::{AgentId, ChannelId, MemoryPoolId, SkillId};
use simulacra_catalog::models::{Agent as AgentModel, AgentPatch, NewAgent, PageRequest};
use simulacra_catalog::repo::{
    AgentFileRepository, AgentRepository, ChannelRepository, MemoryPoolRepository, SkillRepository,
};

use crate::context::GraphQLContext;
use crate::error::to_field_error;
use crate::schema::agent_file::AgentFileNode;
use crate::schema::channel::ChannelNode;
use crate::schema::connection::{PageInfoExt, PageInput, encode_cursor};
use crate::schema::memory_pool::MemoryPoolNode;
use crate::schema::scalars::DateTimeScalar;
use crate::schema::skill::SkillNode;
use crate::schema::tool::{Tool, project_agent_tools};

#[derive(Clone, Debug)]
pub struct AgentNode(pub AgentModel);

#[Object(name = "Agent")]
impl AgentNode {
    async fn id(&self) -> ID {
        ID(self.0.id.0.clone())
    }

    async fn name(&self) -> &str {
        &self.0.name
    }

    async fn description(&self) -> Option<&str> {
        self.0.description.as_deref()
    }

    async fn system_prompt(&self) -> &str {
        &self.0.system_prompt
    }

    async fn model(&self) -> &str {
        &self.0.model
    }

    async fn max_turns(&self) -> i32 {
        self.0.max_turns as i32
    }

    async fn max_tokens(&self) -> Option<i32> {
        self.0.max_tokens.map(|value| value as i32)
    }

    async fn skills(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<SkillNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let skills = ctx.data::<Arc<dyn SkillRepository>>()?;
        let rows = skills
            .list_for_agent(&gql.tenant_id, &self.0.id)
            .await
            .map_err(to_field_error)?;
        Ok(rows.into_iter().map(SkillNode).collect())
    }

    async fn capabilities(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<String>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let agents = ctx.data::<Arc<dyn AgentRepository>>()?;
        agents
            .capabilities(&gql.tenant_id, &self.0.id)
            .await
            .map_err(to_field_error)
    }

    /// S044 — Structured projection of `capabilities[]` into the Tool
    /// catalog. Capability strings that don't resolve are dropped (with a
    /// warn log carrying tenant_id + agent_id + the unknown string).
    async fn tools(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Tool>> {
        project_agent_tools(ctx, &self.0.id).await
    }

    /// S045 — Per-agent files in `created_at, id` ASC order. Empty list,
    /// not null, when the agent has no files.
    async fn files(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<AgentFileNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let files = ctx.data::<Arc<dyn AgentFileRepository>>()?;
        let rows = files
            .list_for_agent(&gql.tenant_id, &self.0.id)
            .await
            .map_err(to_field_error)?;
        Ok(rows.into_iter().map(AgentFileNode).collect())
    }

    /// S046 — Channels the agent listens on, in `created_at, id` ASC
    /// order. Empty list, not null, when the agent has no channels.
    async fn channels(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<ChannelNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let channels = ctx.data::<Arc<dyn ChannelRepository>>()?;
        let rows = channels
            .list_for_agent(&gql.tenant_id, &self.0.id)
            .await
            .map_err(to_field_error)?;
        Ok(rows.into_iter().map(ChannelNode).collect())
    }

    async fn memory_pool(
        &self,
        ctx: &Context<'_>,
    ) -> async_graphql::Result<Option<MemoryPoolNode>> {
        let Some(mpid) = &self.0.memory_pool_id else {
            return Ok(None);
        };
        let gql = ctx.data::<GraphQLContext>()?;
        let pools = ctx.data::<Arc<dyn MemoryPoolRepository>>()?;
        match pools.get(&gql.tenant_id, mpid).await {
            Ok(p) => Ok(Some(MemoryPoolNode(p))),
            Err(CatalogError::NotFound(_)) => Ok(None),
            Err(e) => Err(to_field_error(e)),
        }
    }

    async fn created_at(&self) -> DateTimeScalar {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTimeScalar {
        self.0.updated_at
    }
}

#[derive(SimpleObject, Clone, Debug)]
pub struct AgentEdge {
    pub node: AgentNode,
    pub cursor: String,
}

#[derive(SimpleObject, Clone, Debug)]
pub struct AgentConnection {
    pub edges: Vec<AgentEdge>,
    #[graphql(name = "pageInfo")]
    pub page_info: PageInfoExt,
}

#[derive(InputObject, Clone, Debug, Default)]
pub struct AgentFilter {
    pub name_contains: Option<String>,
}

#[derive(InputObject, Clone, Debug)]
pub struct CreateAgentInput {
    pub name: String,
    pub description: Option<String>,
    pub system_prompt: String,
    pub model: String,
    pub max_turns: Option<i32>,
    pub max_tokens: Option<i32>,
    pub skill_ids: Vec<ID>,
    pub capabilities: Vec<String>,
    pub memory_pool_id: Option<ID>,
    /// S046 — channels the agent listens on. Default `[]` (no channels).
    #[graphql(default)]
    pub channel_ids: Vec<ID>,
}

#[derive(InputObject, Clone, Debug, Default)]
pub struct UpdateAgentInput {
    pub description: MaybeUndefined<String>,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub max_turns: Option<i32>,
    pub max_tokens: MaybeUndefined<i32>,
    pub skill_ids: MaybeUndefined<Vec<ID>>,
    pub capabilities: MaybeUndefined<Vec<String>>,
    pub memory_pool_id: MaybeUndefined<ID>,
    /// S046 — `[..]` replaces the binding atomically; absent/null leaves
    /// channels unchanged. Mirrors `skill_ids` semantics.
    pub channel_ids: MaybeUndefined<Vec<ID>>,
}

#[derive(Default)]
pub struct AgentQuery;

#[Object]
impl AgentQuery {
    async fn agent(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<Option<AgentNode>> {
        let gql = ctx.data::<GraphQLContext>()?;
        let agents = ctx.data::<Arc<dyn AgentRepository>>()?;
        let aid = AgentId(id.to_string());
        match agents.get(&gql.tenant_id, &aid).await {
            Ok(a) => Ok(Some(AgentNode(a))),
            Err(CatalogError::NotFound(_)) => Ok(None),
            Err(e) => Err(to_field_error(e)),
        }
    }

    async fn agents(
        &self,
        ctx: &Context<'_>,
        filter: Option<AgentFilter>,
        page: Option<PageInput>,
    ) -> async_graphql::Result<AgentConnection> {
        let gql = ctx.data::<GraphQLContext>()?;
        let agents = ctx.data::<Arc<dyn AgentRepository>>()?;
        let req = page_request_from(page);
        // Push the `nameContains` filter down into the repo so pageInfo
        // reflects the filtered universe. Treat empty/whitespace-only needles
        // as "no filter" (the historical post-pagination shim ignored them).
        let needle: Option<String> = filter
            .and_then(|f| f.name_contains)
            .filter(|s| !s.is_empty());
        let result = agents
            .list(&gql.tenant_id, req, needle.as_deref())
            .await
            .map_err(to_field_error)?;

        let edges: Vec<AgentEdge> = result
            .items
            .into_iter()
            .map(|a: AgentModel| {
                let cursor = encode_cursor(a.created_at, a.id.as_str());
                AgentEdge {
                    node: AgentNode(a),
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

        Ok(AgentConnection { edges, page_info })
    }
}

pub(crate) fn page_request_from(page: Option<PageInput>) -> PageRequest {
    let p = page.unwrap_or_default();
    PageRequest {
        first: p.first.map(|n| n.max(0) as u32),
        after: p.after,
        last: p.last.map(|n| n.max(0) as u32),
        before: p.before,
    }
}

#[derive(Default)]
pub struct AgentMutation;

#[Object]
impl AgentMutation {
    async fn create_agent(
        &self,
        ctx: &Context<'_>,
        input: CreateAgentInput,
    ) -> async_graphql::Result<AgentNode> {
        let gql = ctx.data::<GraphQLContext>()?;
        let agents = ctx.data::<Arc<dyn AgentRepository>>()?;

        let skill_ids: Vec<SkillId> = input
            .skill_ids
            .iter()
            .map(|id| SkillId(id.to_string()))
            .collect();
        let capabilities: Vec<String> = input.capabilities.clone();
        let memory_pool_id = input
            .memory_pool_id
            .as_ref()
            .map(|id| MemoryPoolId(id.to_string()));

        let channel_ids: Vec<ChannelId> = input
            .channel_ids
            .iter()
            .map(|id| ChannelId(id.to_string()))
            .collect();

        let new_agent = NewAgent {
            name: &input.name,
            description: input.description.as_deref(),
            system_prompt: &input.system_prompt,
            model: &input.model,
            max_turns: input.max_turns.map(|n| n.max(0) as u32),
            max_tokens: input.max_tokens.map(|n| n.max(0) as u32),
            memory_pool_id: memory_pool_id.as_ref(),
            skill_ids: &skill_ids,
            capabilities: &capabilities,
            channel_ids: &channel_ids,
        };

        let agent = agents
            .create(&gql.tenant_id, new_agent)
            .await
            .map_err(to_field_error)?;
        Ok(AgentNode(agent))
    }

    async fn update_agent(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: UpdateAgentInput,
    ) -> async_graphql::Result<AgentNode> {
        let gql = ctx.data::<GraphQLContext>()?;
        let agents = ctx.data::<Arc<dyn AgentRepository>>()?;
        let aid = AgentId(id.to_string());

        let description: Option<Option<String>> = match input.description {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(None),
            MaybeUndefined::Value(v) => Some(Some(v)),
        };
        let max_tokens: Option<Option<u32>> = match input.max_tokens {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(None),
            MaybeUndefined::Value(v) => Some(Some(v.max(0) as u32)),
        };
        let memory_pool_owned: Option<Option<MemoryPoolId>> = match input.memory_pool_id {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => Some(None),
            MaybeUndefined::Value(v) => Some(Some(MemoryPoolId(v.to_string()))),
        };
        // Per spec line 538-540 the test contract is "absent OR null preserves;
        // empty clears": treat both Undefined and Null as no-change.
        let skill_ids_owned: Option<Vec<SkillId>> = match input.skill_ids {
            MaybeUndefined::Undefined | MaybeUndefined::Null => None,
            MaybeUndefined::Value(v) => {
                Some(v.into_iter().map(|id| SkillId(id.to_string())).collect())
            }
        };
        let capabilities_owned: Option<Vec<String>> = match input.capabilities {
            MaybeUndefined::Undefined | MaybeUndefined::Null => None,
            MaybeUndefined::Value(v) => Some(v),
        };
        let channel_ids_owned: Option<Vec<ChannelId>> = match input.channel_ids {
            MaybeUndefined::Undefined | MaybeUndefined::Null => None,
            MaybeUndefined::Value(v) => {
                Some(v.into_iter().map(|id| ChannelId(id.to_string())).collect())
            }
        };

        let patch = AgentPatch {
            description: description.as_ref().map(|inner| inner.as_deref()),
            system_prompt: input.system_prompt.as_deref(),
            model: input.model.as_deref(),
            max_turns: input.max_turns.map(|n| n.max(0) as u32),
            max_tokens,
            memory_pool_id: memory_pool_owned.as_ref().map(|inner| inner.as_ref()),
            skill_ids: skill_ids_owned.as_deref(),
            capabilities: capabilities_owned.as_deref(),
            channel_ids: channel_ids_owned.as_deref(),
        };

        let agent = agents
            .update(&gql.tenant_id, &aid, patch)
            .await
            .map_err(to_field_error)?;
        Ok(AgentNode(agent))
    }

    async fn delete_agent(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<bool> {
        let gql = ctx.data::<GraphQLContext>()?;
        let agents = ctx.data::<Arc<dyn AgentRepository>>()?;
        let aid = AgentId(id.to_string());
        agents
            .delete(&gql.tenant_id, &aid)
            .await
            .map_err(to_field_error)?;
        Ok(true)
    }
}
