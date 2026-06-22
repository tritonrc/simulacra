# S046 — Channels (v1)

> **Status:** Draft
>
> **Related:** S042 (agent catalog + GraphQL), S045 (per-agent files), S031 (api server, webhooks)
>
> **Form field this unblocks:** "Channels" multi-select in the agent-builder form.

## Summary

A `Channel` is a named entry point an agent can listen on — Slack channel, Teams channel, email inbox, webhook URL, manual ("ad-hoc human starts a task"). v1 lands the **data model + control surface** so the agent-builder form can pick channels and bind them to an agent. v1 does NOT route messages from external systems through channels; that integration arrives in S047+.

The minimal split:
- **v1 (this spec):** catalog table, GraphQL CRUD, `Agent.channels` join, `updateAgent { channelIds }`. Read-only at runtime — `ResolvedAgent` carries the list, but no channel-driven dispatch.
- **v2+ (later specs):** webhook receiver routes a payload to "the agent listening on channel X", Slack/Teams/Email adapters, channel-scoped policy, channel auth credentials.

## Authority

- `ARCHITECTURE.md`: agents are bound to identifiers; capability scopes determine what they can do.
- `S042`: agent catalog is the system of record for agent metadata. Extending it with channels follows the same pattern as `skill_ids`.

## Out of scope (v1)

- No actual message delivery. Slack/Teams/Email adapters are S047+.
- No channel-scoped credentials yet (those join the integration fabric in S033).
- No per-channel policy / hooks. (Tenant-level governance still applies.)
- No bi-directional channels. v1 records *binding*, not direction.
- No `kind`-specific config validation. The `config` JSON column is opaque to v1.

## Data model

### `channels` table

```sql
CREATE TABLE channels (
  id           TEXT PRIMARY KEY,
  tenant_id    TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name         TEXT NOT NULL,
  kind         TEXT NOT NULL,          -- 'slack' | 'teams' | 'email' | 'webhook' | 'manual'
  config       TEXT NOT NULL,           -- JSON, kind-specific, opaque to v1
  created_at   TIMESTAMP NOT NULL,
  updated_at   TIMESTAMP NOT NULL,
  UNIQUE (tenant_id, name)
);
CREATE INDEX idx_channels_tenant ON channels(tenant_id);
```

### `agent_channels` join

```sql
CREATE TABLE agent_channels (
  agent_id     TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  channel_id   TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  PRIMARY KEY (agent_id, channel_id)
);
CREATE INDEX idx_agent_channels_channel ON agent_channels(channel_id);
```

Mirrors `agent_skills`. Tenant scoping enforced at the repo layer (an agent's channel must belong to the same tenant — repo write rejects with `Validation` if not).

### `Channel` model

```rust
pub struct Channel {
    pub id: ChannelId,
    pub tenant_id: TenantId,
    pub name: String,
    pub kind: ChannelKind,
    pub config: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub enum ChannelKind { Slack, Teams, Email, Webhook, Manual }
```

`ChannelKind` serializes to its lowercase name (`"slack"`, etc.). The `config` field is opaque JSON — no v1 schema. We accept and round-trip whatever the form sends.

### `ResolvedAgent` extension

`ResolvedAgent` gains `channels: Vec<Channel>`. `AgentRepository::resolve` joins them inside the transaction. Same snapshot-at-spawn semantics as `skills` and `files` (S042 §6).

## Repository surface

```rust
#[async_trait]
pub trait ChannelRepository: Send + Sync {
    async fn create(&self, &TenantId, NewChannel<'_>) -> Result<Channel, CatalogError>;
    async fn get(&self, &TenantId, &ChannelId) -> Result<Channel, CatalogError>;
    async fn list(&self, &TenantId, PageRequest, name_contains: Option<&str>)
        -> Result<Page<Channel>, CatalogError>;
    async fn update(&self, &TenantId, &ChannelId, ChannelPatch<'_>) -> Result<Channel, CatalogError>;
    async fn delete(&self, &TenantId, &ChannelId) -> Result<(), CatalogError>;
    async fn list_for_agent(&self, &TenantId, &AgentId) -> Result<Vec<Channel>, CatalogError>;
}
```

`AgentPatch` gains an optional `channel_ids: Option<&[ChannelId]>` (mirrors `skill_ids`). When `Some`, replaces the join rows; when `None`, leaves channels alone.

`NewAgent` gains `channel_ids: &[ChannelId]` (parallel to `skill_ids`). Empty slice = no channels.

## GraphQL surface

```graphql
type Channel {
  id: ID!
  tenantId: ID!
  name: String!
  kind: ChannelKind!
  config: JSON!
  createdAt: DateTime!
  updatedAt: DateTime!
}

enum ChannelKind { SLACK TEAMS EMAIL WEBHOOK MANUAL }

type Agent {
  channels: [Channel!]!     # NEW. Already-existing fields unchanged.
}

type Query {
  channel(id: ID!): Channel
  channels(filter: ChannelFilter, page: PageInput): ChannelConnection!
}

type Mutation {
  createChannel(input: CreateChannelInput!): Channel!
  updateChannel(id: ID!, input: UpdateChannelInput!): Channel!
  deleteChannel(id: ID!): Boolean!
  # updateAgent already exists; gains:
  #   channelIds: [ID!]    (MaybeUndefined — same semantics as skillIds)
}

input CreateChannelInput {
  name: String!
  kind: ChannelKind!
  config: JSON
}

input UpdateChannelInput {
  name: String
  kind: ChannelKind
  config: JSON
}

input ChannelFilter { nameContains: String }
```

`createAgent` ALSO gains `channelIds: [ID!]` (default `[]`) so a freshly-created agent can be channel-bound on first save.

## Behavior

### Create / Update / Delete

- `createChannel` validates name uniqueness within the tenant; duplicate name → `Conflict` → GraphQL `CONFLICT` error.
- `updateChannel` is a partial patch; `MaybeUndefined::Null` for `name` is rejected (`name` is required), but JSON `null` for `config` clears the config to `{}`.
- `deleteChannel` cascades to `agent_channels` (FK ON DELETE CASCADE). The agents themselves stay; their `channels` list shrinks.

### Tenant scoping

- Cross-tenant `channel(id)` returns null. Cross-tenant `updateChannel`/`deleteChannel` returns `NotFound`.
- `updateAgent { channelIds: [foreign_channel_id] }` rejects with `Validation` ("channel does not belong to this tenant").

### `Agent.channels`

Returns rows joined via `agent_channels`, sorted `(created_at, id)` ASC. Empty list, not null, when an agent has no channels.

## Assertions

### Catalog
- [ ] Migration 0003 creates `channels` and `agent_channels` tables and the `idx_channels_tenant` and `idx_agent_channels_channel` indexes.
- [ ] `ChannelRepository::create` populates id + timestamps and persists `kind` + `config` JSON verbatim.
- [ ] `create` with duplicate name in tenant → `Conflict`.
- [ ] `get` returns the channel for the correct tenant; cross-tenant returns `NotFound`.
- [ ] `list` paginates `(created_at, id)` ASC with optional `name_contains` push-down (not post-filter).
- [ ] `update` round-trips a JSON `config` patch; setting `name: null` returns `Validation`.
- [ ] `delete` cascades to `agent_channels` (after delete, no row references the deleted channel).
- [ ] `list_for_agent` returns the joined channels in `(created_at, id)` ASC order.
- [ ] `AgentRepository::resolve` carries `channels: Vec<Channel>` populated from the join.
- [ ] `createAgent` with `channel_ids` containing a foreign-tenant channel → `Validation`.
- [ ] `updateAgent` with `channel_ids: Some([..])` replaces join rows atomically (the new set, not a union).
- [ ] `updateAgent` with `channel_ids: None` leaves join rows untouched.

### GraphQL
- [ ] `Channel` type returns all fields including `config: JSON`.
- [ ] `channel(id)` returns null for unknown id and for cross-tenant id (no errors, no leak).
- [ ] `channels(page, filter)` returns a Connection with stable cursor ordering.
- [ ] `createChannel` writes a row + returns it; duplicate name → `CONFLICT` GraphQL error.
- [ ] `updateChannel(id, { name, kind, config })` round-trips each field.
- [ ] `deleteChannel(id)` returns `true`; cross-tenant returns `false` without errors.
- [ ] `Agent.channels` returns the agent's bound channels; empty list when none.
- [ ] `updateAgent(id, { channelIds: [..] })` replaces the binding atomically and is reflected in `Agent.channels`.

### v2+ (deferred — NOT v1)
- Webhook routing: when a webhook hits a route bound to channel X, the engine spawns the agent listening on channel X. (Spec S047.)
- Slack/Teams/Email adapters that subscribe to upstream events and feed them into channel routing.
- Per-channel auth credentials integrated with S033 integration fabric.

## Why this is shippable on its own

- Backend-only. The form persists channel bindings against this surface; the runtime currently ignores them — `ResolvedAgent.channels` is wired but no spawn path consumes it yet.
- No external dependencies. SQLite migration + GraphQL types only.
- Test coverage is purely behavioral: catalog round-trip + GraphQL CRUD against an in-memory catalog.
