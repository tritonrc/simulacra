use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{AgentFileId, AgentId, ChannelId, MemoryPoolId, SkillId, TenantId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tenant {
    pub id: TenantId,
    pub namespace: String,
    pub display_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub tenant_id: TenantId,
    pub name: String,
    pub description: Option<String>,
    pub system_prompt: String,
    pub model: String,
    pub max_turns: u32,
    pub max_tokens: Option<u32>,
    pub memory_pool_id: Option<MemoryPoolId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub tenant_id: TenantId,
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    pub metadata: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryPool {
    pub id: MemoryPoolId,
    pub tenant_id: TenantId,
    pub name: String,
    pub embedding_model: Option<String>,
    pub config: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedAgent {
    pub id: AgentId,
    pub name: String,
    pub system_prompt: String,
    pub model: String,
    pub max_turns: u32,
    pub max_tokens: Option<u32>,
    pub skills: Vec<Skill>,
    pub capabilities: Vec<String>,
    pub memory_pool: Option<MemoryPool>,
    pub files: Vec<AgentFile>,
    /// S046 — channels the agent listens on. v1 is purely informational
    /// (no runtime dispatch). Snapshot at spawn time, same as `skills`.
    pub channels: Vec<Channel>,
}

/// S046 — Channel kind discriminator. Serializes to lowercase
/// (`"slack"`, etc.) for SQL TEXT and JSON. v1 doesn't validate the
/// kind-specific `config` payload — that's deferred to S047+ adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Slack,
    Teams,
    Email,
    Webhook,
    Manual,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Slack => "slack",
            ChannelKind::Teams => "teams",
            ChannelKind::Email => "email",
            ChannelKind::Webhook => "webhook",
            ChannelKind::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "slack" => Some(ChannelKind::Slack),
            "teams" => Some(ChannelKind::Teams),
            "email" => Some(ChannelKind::Email),
            "webhook" => Some(ChannelKind::Webhook),
            "manual" => Some(ChannelKind::Manual),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub tenant_id: TenantId,
    pub name: String,
    pub kind: ChannelKind,
    pub config: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct NewChannel<'a> {
    pub name: &'a str,
    pub kind: ChannelKind,
    pub config: Option<&'a Value>,
}

/// Partial patch. All fields are `Option` of the underlying type.
/// Setting `name` or `kind` to `None` means "leave unchanged" (the GraphQL
/// surface rejects `null` for these).
/// `config: Some(None)` clears the config to `{}`; `config: Some(Some(v))`
/// replaces; `config: None` leaves it.
#[derive(Clone, Debug, Default)]
pub struct ChannelPatch<'a> {
    pub name: Option<&'a str>,
    pub kind: Option<ChannelKind>,
    pub config: Option<Option<&'a Value>>,
}

/// S045 — Per-agent file metadata. Bytes live in `agent_file_bytes`,
/// reachable via [`crate::repo::AgentFileRepository::read_bytes`] or the
/// underlying [`crate::AgentFileStore`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentFile {
    pub id: AgentFileId,
    pub agent_id: AgentId,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct NewAgentFile<'a> {
    pub agent_id: &'a AgentId,
    pub name: &'a str,
    pub mime_type: &'a str,
    pub bytes: &'a [u8],
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub end_cursor: Option<String>,
    pub start_cursor: Option<String>,
    pub has_next_page: bool,
    pub has_previous_page: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageRequest {
    pub first: Option<u32>,
    pub after: Option<String>,
    pub last: Option<u32>,
    pub before: Option<String>,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            first: Some(20),
            after: None,
            last: None,
            before: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NewAgent<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub system_prompt: &'a str,
    pub model: &'a str,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u32>,
    pub memory_pool_id: Option<&'a MemoryPoolId>,
    pub skill_ids: &'a [SkillId],
    pub capabilities: &'a [String],
    /// S046 — channels the agent listens on. Empty slice = none.
    pub channel_ids: &'a [ChannelId],
}

#[derive(Clone, Debug, Default)]
pub struct AgentPatch<'a> {
    pub description: Option<Option<&'a str>>,
    pub system_prompt: Option<&'a str>,
    pub model: Option<&'a str>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<Option<u32>>,
    pub memory_pool_id: Option<Option<&'a MemoryPoolId>>,
    pub skill_ids: Option<&'a [SkillId]>,
    pub capabilities: Option<&'a [String]>,
    /// S046 — `Some(&[..])` replaces the join rows; `None` leaves them.
    pub channel_ids: Option<&'a [ChannelId]>,
}

#[derive(Clone, Debug, Default)]
pub struct NewSkill<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub body: &'a str,
    pub metadata: Option<&'a Value>,
}

#[derive(Clone, Debug, Default)]
pub struct SkillPatch<'a> {
    pub name: Option<&'a str>,
    pub description: Option<Option<&'a str>>,
    pub body: Option<&'a str>,
    pub metadata: Option<Option<&'a Value>>,
}

#[derive(Clone, Debug)]
pub struct NewMemoryPool<'a> {
    pub name: &'a str,
    pub embedding_model: Option<&'a str>,
    pub config: &'a Value,
}

#[derive(Clone, Debug, Default)]
pub struct MemoryPoolPatch<'a> {
    pub name: Option<&'a str>,
    pub embedding_model: Option<Option<&'a str>>,
    pub config: Option<&'a Value>,
}
