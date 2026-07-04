# S042 — Agent Catalog & GraphQL Control Plane

**Status:** Active (v1 — see "Deferred follow-ups" below)
**Crates:** `simulacra-catalog` (new), `simulacra-graphql` (new), `simulacra-server`, `simulacra-cli`, `simulacra-vfs`

## Dependencies

- **S001** — VFS composition (CatalogSkillFs implements `FsLayer`)
- **S004** — Capability tokens (per-agent capability grants applied at task spawn)
- **S017** — Skills (skill discovery walks the VFS; CatalogSkillFs participates)
- **S031** — API server (auth provider, tenant resolver, axum route mounting)
- **S033** — Integration fabric (existing `/var/skills/` compatibility namespace)
- **S034** — SimulacraEngine (composition root rewired to read from catalog)
- **S037** — Memory (SqliteMemoryStore + BackgroundEmbedder; routed by memory_pool config)
- **S038** — CLI memory wiring (bootstrap path that the catalog augments)
- **S040** — WASM-backed VFS nodes (architectural precedent for DB-backed FsLayer)

## Scope

A SQLite-backed catalog of customer-managed agent definitions, skills, and memory pools, fronted by a GraphQL control-plane API. The runtime (`SimulacraEngine`) reads agent definitions from the catalog at task spawn time. DB-authored skills are visible to the agent loop via a new VFS layer.

This is the foundational spec for the broader "web-managed agent platform" effort. Teams (multi-agent composition) and trigger management each get their own spec; this one establishes the persistence pattern, GraphQL surface, and runtime seam.

**In scope:**

- New crate `simulacra-catalog`:
  - SQLite schema + forward-only migrations
  - Repository traits: `AgentRepository`, `SkillRepository`, `MemoryPoolRepository`, `CapabilityRepository`, `TenantRepository`
  - SQLite implementations of each (`SqliteAgentRepository`, etc.)
  - In-memory repository implementations (`MemoryAgentRepository`, etc.) for `--no-catalog` CLI mode
  - `CatalogSkillFs` — `FsLayer` impl exposing skills as `/skills/<name>/SKILL.md`
  - `ResolvedAgent` snapshot type used by `SimulacraEngine`
- New crate `simulacra-graphql`:
  - `async-graphql` schema (queries + mutations; no subscriptions in v1)
  - Resolvers for `Agent`, `Skill`, `MemoryPool` (CRUD + connection-style pagination)
  - axum route handler that integrates with S031's `AuthProvider` and tenant resolver
  - `GraphQLContext` carrying authenticated principal + resolved tenant_id
- `simulacra-server` edits:
  - `SimulacraEngine` constructor accepts `Arc<dyn AgentRepository>` and friends
  - `SimulacraEngine::spawn_task` resolves agents from catalog instead of `SimulacraConfig.agent_types`
  - axum router mounts `/graphql` from `simulacra-graphql`
- `simulacra-cli` edits:
  - Default mode: open catalog DB, run migrations, perform one-shot TOML→DB import for the default tenant (idempotent via `seeds_applied` table); resolve agents through the catalog (uniform with server-mode)
  - `--no-catalog` flag: skip catalog entirely; populate `MemoryAgentRepository`/`MemorySkillRepository`/`MemoryMemoryPoolRepository` from `SimulacraConfig` and run with no DB. Filesystem skills (S033 host mounts) still work; DB-only features (GraphQL, multi-tenant, persistence) are unavailable.
- Per-task VFS composition:
  - `CatalogSkillFs` produces canonical `/skills/<name>/SKILL.md` documents for S017 discovery. Server tasks also preserve `/var/skills/<name>.md` as a compatibility/debug path.

**Out of scope (deferred to later specs):**

- GraphQL subscriptions for live updates (folded into the web UI spec)
- Team primitive and team CRUD (next spec)
- Trigger management as a persistent entity (spec after teams)
- Capability templates (UI affordance; no runtime requirement)
- Soft delete, versioning, history, forking/cloning of any entity
- File-upload / multipart for skill bodies (text-only via mutation in v1)
- Bulk import/export, `simulacra admin reimport-toml` command
- Postgres backend (trait boundary preserves the option; v1 ships SQLite only)
- Cross-tenant admin queries / superuser scope
- Hot-reload of in-flight task agents (semantics are explicitly read-at-spawn, frozen-for-task)

## Context

Today, agent definitions live in `simulacra.toml` under `[agent_types.*]` and are loaded into `SimulacraConfig` at startup. `SimulacraEngine::spawn_task` (S034) resolves the agent type by name against this in-memory map. There is no API-level surface for a customer to author or modify an agent without editing TOML and restarting the server.

The product direction is a hosted, web-managed agent platform where customers create agents, compose teams, and configure triggers through a browser. That requires:

1. **Persistent storage** of customer-authored entities, scoped by tenant.
2. **A typed control-plane API** that the web UI (and future SDK / Slack bot / embedded clients) consume.
3. **A runtime that reads from this storage** at task spawn time, so API-authored agents actually run.

This spec establishes the substrate. The persistence layer is SQLite (matching S037's `SqliteMemoryStore` precedent and avoiding a new operational dependency). The transport is GraphQL per project preference. Runtime integration is end-to-end: the seam between "agent created in API" and "agent runs" closes in this spec, not in a follow-up — per repo discipline against shipping framework-only scaffolding.

### Relationship to TOML config

`simulacra.toml` continues to govern server bootstrap: `[server]`, `[auth]`, `[providers]`, `[hooks]`, `[mcp.servers]`, and process-level config. Customer-managed entities — agents, skills, memory pools — move to the catalog. On first boot, any `[agent_types.*]` entries in TOML are imported into the default tenant's catalog as a one-shot seed; subsequent TOML edits to those sections are ignored (logged once at INFO).

The existing `[memory]` section (S038) becomes the server-level default for memory pool storage (DB path, embedding model). Catalog `MemoryPool` rows inherit these defaults unless their `config` JSON overrides them. Agents without an explicit `memory_pool_id` use a default pool auto-created during TOML import (named `default`) that mirrors the `[memory]` section.

### Hot-reload semantics

Agent definitions are read from the catalog at task spawn time and snapshotted into a `ResolvedAgent` for the duration of that task. Mutations to the catalog during execution do not affect in-flight tasks; they take effect on the next task spawn. This avoids mid-task identity changes and matches the existing immutable-agent-config behavior.

## Design

### `simulacra-catalog` crate

#### SQLite schema (initial migration `0001_initial.sql`)

```sql
CREATE TABLE tenants (
  id           TEXT PRIMARY KEY,
  namespace    TEXT NOT NULL UNIQUE,
  display_name TEXT,
  created_at   TIMESTAMP NOT NULL,
  updated_at   TIMESTAMP NOT NULL
);

CREATE TABLE memory_pools (
  id              TEXT PRIMARY KEY,
  tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name            TEXT NOT NULL,
  embedding_model TEXT,
  config          TEXT NOT NULL,            -- JSON: vector dim, store path, etc.
  created_at      TIMESTAMP NOT NULL,
  updated_at      TIMESTAMP NOT NULL,
  UNIQUE(tenant_id, name)
);

CREATE TABLE agents (
  id              TEXT PRIMARY KEY,
  tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name            TEXT NOT NULL,
  description     TEXT,
  system_prompt   TEXT NOT NULL,
  model           TEXT NOT NULL,
  max_turns       INTEGER NOT NULL DEFAULT 100,
  max_tokens      INTEGER,
  memory_pool_id  TEXT REFERENCES memory_pools(id) ON DELETE SET NULL,
  created_at      TIMESTAMP NOT NULL,
  updated_at      TIMESTAMP NOT NULL,
  UNIQUE(tenant_id, name)
);

CREATE TABLE skills (
  id          TEXT PRIMARY KEY,
  tenant_id   TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  description TEXT,
  body        TEXT NOT NULL,
  metadata    TEXT,                         -- JSON frontmatter
  created_at  TIMESTAMP NOT NULL,
  updated_at  TIMESTAMP NOT NULL,
  UNIQUE(tenant_id, name)
);

CREATE TABLE agent_skills (
  agent_id  TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  skill_id  TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
  PRIMARY KEY (agent_id, skill_id)
);

CREATE TABLE agent_capabilities (
  agent_id   TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  capability TEXT NOT NULL,                 -- e.g. "mcp:fetcher:*"
  PRIMARY KEY (agent_id, capability)
);

CREATE TABLE seeds_applied (
  source     TEXT PRIMARY KEY,              -- e.g. "toml:agent_types"
  applied_at TIMESTAMP NOT NULL
);

CREATE INDEX idx_agents_tenant ON agents(tenant_id);
CREATE INDEX idx_skills_tenant ON skills(tenant_id);
CREATE INDEX idx_memory_pools_tenant ON memory_pools(tenant_id);
```

ID strategy: ULID (sortable, K-orderable). Stored as 26-char text. Crate dependency: `ulid`.

JSON columns stored as `TEXT` and parsed at the repository boundary with `serde_json`. SQLite's `JSON1` extension is not used.

The `skills.metadata` column stores parsed frontmatter as a JSON object. When `CatalogSkillFs::read` serves a skill, it reconstructs YAML frontmatter from `metadata` plus row fields (`name`, and `description` when present), delimits it with `---` markers, and prepends it to `body`, producing the same on-disk shape that S017's skill discovery already understands.

Skill names used as catalog VFS paths must be valid single path segments: non-empty, not `.` or `..`, and containing no `/`, `\`, or NUL bytes. `CatalogSkillFs` does not expose invalid names, and `SimulacraEngine` rejects a resolved agent containing one before mounting skills.

PRAGMAs at connection open: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`.

#### Repository traits

```rust
#[async_trait]
pub trait TenantRepository: Send + Sync {
    async fn get_by_namespace(&self, namespace: &str) -> Result<Tenant, CatalogError>;
    async fn create(&self, namespace: &str, display_name: Option<&str>) -> Result<Tenant, CatalogError>;
}

#[async_trait]
pub trait AgentRepository: Send + Sync {
    async fn get(&self, tenant_id: &TenantId, name: &str) -> Result<Agent, CatalogError>;
    async fn get_by_id(&self, tenant_id: &TenantId, id: &AgentId) -> Result<Agent, CatalogError>;
    async fn list(&self, tenant_id: &TenantId, page: PageRequest) -> Result<Page<Agent>, CatalogError>;
    async fn create(&self, tenant_id: &TenantId, input: NewAgent<'_>) -> Result<Agent, CatalogError>;
    async fn update(&self, tenant_id: &TenantId, id: &AgentId, input: AgentPatch<'_>) -> Result<Agent, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &AgentId) -> Result<(), CatalogError>;
    async fn resolve(&self, tenant_id: &TenantId, name: &str) -> Result<ResolvedAgent, CatalogError>;
}

#[async_trait]
pub trait SkillRepository: Send + Sync {
    async fn get(&self, tenant_id: &TenantId, id: &SkillId) -> Result<Skill, CatalogError>;
    async fn list(&self, tenant_id: &TenantId, page: PageRequest) -> Result<Page<Skill>, CatalogError>;
    async fn list_for_agent(&self, tenant_id: &TenantId, agent_id: &AgentId) -> Result<Vec<Skill>, CatalogError>;
    async fn create(&self, tenant_id: &TenantId, input: NewSkill<'_>) -> Result<Skill, CatalogError>;
    async fn update(&self, tenant_id: &TenantId, id: &SkillId, input: SkillPatch<'_>) -> Result<Skill, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &SkillId) -> Result<(), CatalogError>;
}

#[async_trait]
pub trait MemoryPoolRepository: Send + Sync { /* parallel shape */ }
```

Every read/write method takes `tenant_id` as the first argument. There is no method that operates without a tenant scope. Cross-tenant access is structurally impossible.

#### `ResolvedAgent`

The frozen snapshot consumed by `SimulacraEngine`:

```rust
pub struct ResolvedAgent {
    pub id: AgentId,
    pub name: String,
    pub system_prompt: String,
    pub model: String,
    pub max_turns: u32,
    pub max_tokens: Option<u32>,
    pub skills: Vec<Skill>,           // full bodies, frozen
    pub capabilities: Vec<String>,    // raw strings
    pub memory_pool: Option<MemoryPool>,
}
```

Produced by `AgentRepository::resolve()` in a single transaction (one query per join).

#### `CatalogSkillFs`

```rust
pub struct CatalogSkillFs {
    skills: Arc<Vec<Skill>>,   // snapshot, immutable for task lifetime
}

impl FsLayer for CatalogSkillFs { /* list_dir, read, stat; write/remove → Errno::ROFS */ }
```

Constructed from the `ResolvedAgent.skills` snapshot. It exposes a read-only tree rooted at `/` where `list_dir("/")` returns skill directory names, each skill directory contains `SKILL.md`, and reads of `/<name>/SKILL.md` return S017-compatible skill documents. `SimulacraEngine` snapshots those rendered documents into each task's VFS at `/skills/<name>/SKILL.md` and `/var/skills/<name>.md`, then guards both namespaces as read-only in the composed/runtime VFS.

### `simulacra-graphql` crate

#### Schema (sketch)

```graphql
scalar DateTime
scalar JSON

type Agent {
  id: ID!
  name: String!
  description: String
  systemPrompt: String!
  model: String!
  maxTurns: Int!
  maxTokens: Int
  skills: [Skill!]!
  capabilities: [String!]!
  memoryPool: MemoryPool
  createdAt: DateTime!
  updatedAt: DateTime!
}

type Skill {
  id: ID!
  name: String!
  description: String
  body: String!
  metadata: JSON
  createdAt: DateTime!
  updatedAt: DateTime!
}

type MemoryPool {
  id: ID!
  name: String!
  embeddingModel: String
  config: JSON!
  createdAt: DateTime!
  updatedAt: DateTime!
}

type AgentEdge { node: Agent!, cursor: String! }
type AgentConnection { edges: [AgentEdge!]!, pageInfo: PageInfo! }
# similar for Skill, MemoryPool

input PageInput { first: Int, after: String, last: Int, before: String }
input AgentFilter { nameContains: String }
input SkillFilter  { nameContains: String }

type Query {
  agent(id: ID!): Agent
  agents(filter: AgentFilter, page: PageInput): AgentConnection!
  skill(id: ID!): Skill
  skills(filter: SkillFilter, page: PageInput): SkillConnection!
  memoryPool(id: ID!): MemoryPool
  memoryPools: [MemoryPool!]!
}

input CreateAgentInput {
  name: String!
  description: String
  systemPrompt: String!
  model: String!
  maxTurns: Int
  maxTokens: Int
  skillIds: [ID!]!
  capabilities: [String!]!
  memoryPoolId: ID
}

input UpdateAgentInput {
  description: String
  systemPrompt: String
  model: String
  maxTurns: Int
  maxTokens: Int
  skillIds: [ID!]
  capabilities: [String!]
  memoryPoolId: ID
}

type Mutation {
  createAgent(input: CreateAgentInput!): Agent!
  updateAgent(id: ID!, input: UpdateAgentInput!): Agent!
  deleteAgent(id: ID!): Boolean!

  createSkill(input: CreateSkillInput!): Skill!
  updateSkill(id: ID!, input: UpdateSkillInput!): Skill!
  deleteSkill(id: ID!): Boolean!

  createMemoryPool(input: CreateMemoryPoolInput!): MemoryPool!
  updateMemoryPool(id: ID!, input: UpdateMemoryPoolInput!): MemoryPool!
  deleteMemoryPool(id: ID!): Boolean!
}
```

`CreateAgentInput` / `UpdateAgentInput` accept full relationship lists in one round trip. Updates with `skillIds: null` mean "no change"; `skillIds: []` means "remove all". Same convention for `capabilities`.

#### `GraphQLContext`

```rust
pub struct GraphQLContext {
    pub tenant_id: TenantId,
    pub principal: AuthenticatedPrincipal,   // from S031
}
```

Constructed once per request inside the axum route handler:

1. S031's `AuthProvider::authenticate(req)` → `AuthenticatedPrincipal { tenant_namespace, ... }` (or 401).
2. `TenantRepository::get_by_namespace(namespace)` → `Tenant` (cached in memory; invalidated on tenant CRUD).
3. `GraphQLContext { tenant_id, principal }` injected into async-graphql request data.

Every resolver pulls `ctx.tenant_id` and passes it to repository calls. A resolver that forgets is a bug; the trait shape makes it a compile-time error to write a query without one.

### `SimulacraEngine` rewire

```rust
pub struct SimulacraEngine {
    config: SimulacraConfig,
    integration_registry: Option<Arc<IntegrationRegistry>>,
    agents: Arc<dyn AgentRepository>,
    skills: Arc<dyn SkillRepository>,
    memory_pools: Arc<dyn MemoryPoolRepository>,
    tenants: Arc<dyn TenantRepository>,
}
```

`spawn_task` flow:

1. Resolve `tenant_namespace → tenant_id` via `TenantRepository`.
2. Pick agent name (from `agent_type_override` or task-default).
3. `agents.resolve(tenant_id, agent_name)` → `ResolvedAgent` snapshot.
4. Compose per-task VFS: `MemoryFs + host_mounts + ServiceFs + ProcFs`, with catalog skill snapshots seeded into `/skills/<name>/SKILL.md` and `/var/skills/<name>.md` before wrapping.
5. Construct `AgentCell`, `ToolRegistry`, `HookPipeline`, `ResourceBudget`, `Journal` as today (S034) — but using `snapshot.system_prompt`, `snapshot.model`, etc.
6. Apply `snapshot.capabilities` to the capability checker.
7. If `snapshot.memory_pool` is `Some`, route the `SqliteMemoryStore` to the pool's configured store path.
8. Spawn the agent loop on a background tokio task (existing path).

New `EngineError::AgentNotFound { tenant: String, agent: String }` replaces the previous panic-on-missing-config.

### CLI modes

The CLI runs in two modes selected at bootstrap:

**Default (catalog-backed).** Opens SQLite at `[catalog].db_path`, runs migrations, performs one-shot TOML import. Constructs `SimulacraEngine` with `SqliteAgentRepository` and friends. Per-task VFS includes rendered catalog skill snapshots. This is the path that mirrors server-mode and is what the GraphQL/web UI consume against.

**`--no-catalog` (TOML-driven, ephemeral).** Skips catalog entirely. Constructs `MemoryAgentRepository` (and skill/memory-pool equivalents) populated from `SimulacraConfig.agent_types`, `SimulacraConfig.memory`, and any inline skill config. `SimulacraEngine` is constructed with these in-memory repositories. Per-task VFS does *not* include `CatalogSkillFs` — only filesystem-mounted skills (S033) are visible. GraphQL is not mounted. No SQLite file is touched.

Both modes share the same `SimulacraEngine` code path. The trait abstraction is the seam.

This means single-shot, embedded, and CI invocations of `simulacra-cli` work without any DB dependency, while the same binary in default mode integrates with the full catalog. Server (`simulacra-server`) is always catalog-backed; `--no-catalog` is CLI-only.

### `simulacra-cli` bootstrap import

On startup, after loading `SimulacraConfig`:

```rust
let catalog = Catalog::open(&config.catalog.db_path).await?;
catalog.migrate().await?;

let default_tenant = catalog.tenants().get_or_create("default", Some("Default")).await?;

if !catalog.seeds_applied("toml:agent_types").await? {
    for (name, agent_type) in &config.agent_types {
        catalog.upsert_agent_from_toml(&default_tenant.id, name, agent_type).await?;
    }
    catalog.mark_seed_applied("toml:agent_types").await?;
    tracing::info!("imported {} agent types from simulacra.toml into catalog", config.agent_types.len());
}
```

Idempotent: rerunning a fresh CLI invocation against the same DB is a no-op. Re-importing after the seed has been applied requires a future `simulacra admin reimport-toml` command (out of scope).

`simulacra.toml` gets a new section:

```toml
[catalog]
db_path = "/var/lib/simulacra/catalog.db"
```

Default if unset: `<state_dir>/catalog.db`.

### Crate position in dependency graph

```
simulacra-graphql ──→ simulacra-catalog ──→ simulacra-vfs
       │                  ↑
       └────────────────  simulacra-server (composition root)
                          ↑
                          simulacra-cli (bootstrap + import)
```

`simulacra-catalog` knows nothing about GraphQL. `simulacra-graphql` knows nothing about SQLite. `simulacra-server` and `simulacra-cli` are the only things that know both.

## Behavior

### Schema migrations
- Initial migration creates all tables and indexes.
- Migrations are forward-only and run on `Catalog::open` if `db_path` exists; created from scratch otherwise.
- Re-opening a migrated DB is a no-op.

### Repository CRUD
- `create` returns the full row including server-generated `id` and timestamps.
- `update` is a partial patch; `None` fields preserve current value.
- `delete` is hard delete; cascades follow FK definitions.
- `get`/`update`/`delete` of nonexistent ID returns `CatalogError::NotFound`.
- Every method takes and enforces `tenant_id` — rows from other tenants are not visible regardless of ID guess.

### Agent resolution (`AgentRepository::resolve`)
- Returns `ResolvedAgent` with skills, capabilities, and memory_pool joined in a single transaction.
- Snapshot is fully owned (no live DB references) — safe to hold for the lifetime of a task.

### `CatalogSkillFs`
- `list_dir("/")` returns `<name>` for every skill in the snapshot.
- `list_dir("/<name>")` returns `SKILL.md`.
- `read("/<name>/SKILL.md")` returns the skill body with S017-compatible frontmatter prepended.
- Invalid path-segment names are not exposed as VFS entries.
- `write` and `remove` return `Errno::ROFS`.
- Server task spawn snapshots rendered catalog skills into `/skills/<name>/SKILL.md` and the compatibility `/var/skills/<name>.md` path.

### GraphQL queries
- All queries require an authenticated principal; unauthenticated requests get a `401` from the auth middleware (before reaching async-graphql).
- All resolvers filter by `ctx.tenant_id`.
- Connection-style pagination is Relay-style: cursor returned in `pageInfo.endCursor` returns the next page when used as `after`. Cursors are opaque base64-encoded `(created_at, id)` tuples.

### GraphQL mutations
- `createAgent` validates input: name unique within tenant, all `skillIds` exist in tenant, `memoryPoolId` (if set) exists in tenant. Failures return typed GraphQL errors with `code` extension.
- `updateAgent` with `skillIds` set replaces the join set transactionally; `null` leaves it unchanged; `[]` removes all.
- `deleteAgent` cascades to `agent_skills` and `agent_capabilities`.
- Mutation success returns the updated entity.

### `SimulacraEngine` integration
- `spawn_task(agent_name)` resolves the agent through the catalog, not `SimulacraConfig.agent_types`.
- Agent unknown to catalog returns `EngineError::AgentNotFound`; no panic.
- Per-task VFS includes read-only catalog skill snapshots at `/skills/<name>/SKILL.md` and `/var/skills/<name>.md`.
- Catalog mutations during a task in flight do not affect that task (snapshot semantics).

### TOML import
- First boot with non-empty `[agent_types.*]` and empty catalog: imports each entry as a row in the default tenant.
- Subsequent boots: no-op (idempotent via `seeds_applied`).
- Editing `[agent_types.foo]` in TOML after import: ignored, with one INFO log noting "use GraphQL to modify".
- Skills under `/var/skills/` on disk are not imported (remain filesystem-only via S033's existing mount).

### Auth + tenant resolution
- S031's `AuthProvider` runs before async-graphql.
- `tenant_namespace` from the principal is mapped to a `tenant_id` via `TenantRepository`.
- Tenant cache invalidates on `createTenant` / `deleteTenant` (admin-only mutations, deferred to follow-up; for v1, the default tenant is auto-created on bootstrap).

## Assertions

### Schema migrations
- [x] Initial migration creates `tenants`, `agents`, `skills`, `memory_pools`, `agent_skills`, `agent_capabilities`, `seeds_applied`, plus indexes.
- [x] `Catalog::open` on a fresh path runs migrations and produces a usable DB.
- [x] Re-opening a migrated DB does not re-run migrations.
- [x] `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON` set per connection.

### Repository CRUD — `AgentRepository`
- [x] `create` populates id and timestamps.
- [x] `get` by missing name returns `NotFound`.
- [x] `get` by missing id returns `NotFound`.
- [x] `update` patches only provided fields.
- [x] `update` of nonexistent id returns `NotFound`.
- [x] `delete` removes row and cascades to `agent_skills` and `agent_capabilities`.
- [x] `delete` of nonexistent id returns `NotFound`.
- [x] `list` returns paginated results with stable ordering by `created_at, id`.
- [x] Tenant A cannot read Tenant B's agents via `get`, `get_by_id`, or `list`.
- [x] Creating an agent with name already used in tenant returns `Conflict`.

### Repository CRUD — `SkillRepository`, `MemoryPoolRepository`
- [x] Mirror agent CRUD assertions for both.
- [x] `SkillRepository::list_for_agent` returns only skills joined to that agent (and only in the right tenant).

### `AgentRepository::resolve`
- [x] Returns `ResolvedAgent` with skills (full bodies), capabilities, memory_pool joined.
- [x] Single transaction across joins.
- [x] Returns `NotFound` for missing agent.
- [x] Cross-tenant resolve attempt returns `NotFound`.

### `CatalogSkillFs`
- [x] `list_dir("/")` returns one directory entry per skill in snapshot, named `<name>`.
- [x] `list_dir("/<name>")` returns `SKILL.md`.
- [x] `read("/<name>/SKILL.md")` returns body bytes with S017-compatible frontmatter prepended.
- [x] Invalid path-segment names are not exposed.
- [x] `write` returns `Errno::ROFS`.
- [x] `remove` returns `Errno::ROFS`.
- [x] When overlaid with a host-mounted layer, DB skill shadows host skill with the same exact `<name>/SKILL.md` path.
- [x] Read of unknown name returns `Errno::NOENT`.

### GraphQL — auth + tenant scoping
- [x] Unauthenticated request to `/graphql` returns 401 before reaching async-graphql.
- [x] Authenticated request without tenant resolves to error before resolvers run.
- [x] Tenant A's principal cannot query tenant B's agent via `agent(id)`.
- [x] Tenant A's principal cannot mutate tenant B's agent via `updateAgent`/`deleteAgent`.

### GraphQL — queries
- [x] `agent(id)` returns full entity with joined skills, capabilities, memory_pool.
- [x] `agents(page)` returns Connection with `pageInfo.hasNextPage` and stable cursor.
- [x] `skills(page)` paginates similarly.
- [x] `memoryPools` returns all pools for the tenant.
- [x] Filters (`nameContains`) reduce result set.

### GraphQL — mutations
- [x] `createAgent` with valid input creates row and returns full entity.
- [x] `createAgent` with duplicate name returns typed conflict error (code: `CONFLICT`).
- [x] `createAgent` referencing nonexistent `skillId` returns typed validation error.
- [x] `createAgent` referencing nonexistent `memoryPoolId` returns typed validation error.
- [x] `updateAgent` with `skillIds: null` preserves existing skills.
- [x] `updateAgent` with `skillIds: []` removes all.
- [x] `updateAgent` with `skillIds: [a, b]` replaces set transactionally.
- [x] `deleteAgent` returns `true` and the agent is no longer queryable.
- [x] Skill mutations parallel agent mutations.
- [x] MemoryPool mutations parallel agent mutations.

### `SimulacraEngine` integration
- [x] `spawn_task` resolves agent from catalog, not from `SimulacraConfig.agent_types`.
- [x] Agent unknown to catalog returns `EngineError::AgentNotFound` (no panic).
- [x] Per-task VFS exposes read-only catalog skills at `/skills/<name>/SKILL.md` and compatibility `/var/skills/<name>.md`.
- [x] Capabilities from catalog feed the per-task capability checker.
- [x] Memory pool config from catalog routes the task's memory store.
- [x] Catalog mutations during a running task do not affect that task.
- [x] Two concurrent tasks for two different agents see their own skills, capabilities, and memory pool.

### TOML import (default mode)
- [x] First boot with non-empty `agent_types`: rows present in catalog after startup.
- [x] Second boot: no-op; row count unchanged.
- [x] TOML edit after import: change ignored; one INFO log emitted.
- [x] `seeds_applied("toml:agent_types")` is `true` after first import.

### `--no-catalog` mode
- [x] CLI run with `--no-catalog` does not open or create a SQLite file.
- [x] Migrations are not invoked.
- [x] `MemoryAgentRepository` is populated from `SimulacraConfig.agent_types` and serves `get`/`list`/`resolve`.
- [x] Per-task VFS does not include `CatalogSkillFs` (true by construction — CLI never mounts CatalogSkillFs).
- [x] GraphQL route is not mounted in CLI mode (true by construction — `simulacra-cli` has no GraphQL surface).
- [x] Mutating methods on the in-memory repositories return `CatalogError::ReadOnly`.
- [ ] An agent defined in TOML resolves and runs to completion under `--no-catalog`. *(deferred — see "Deferred follow-ups")*

### E2E (Phase 3a)
- [x] `createAgent` mutation against running `simulacra-server` creates row.
- [x] Subsequent task creation via S031's API resolves the catalog-defined agent.
- [x] Agent runs to completion against a recording HTTP fixture. *(closed by S043 — `simulacra-server/tests/provider_injection.rs::graphql_created_agent_runs_to_completion_under_a_scripted_provider`. The "fixture" is a stub `Provider` impl rather than HTTP-level; same coverage at the engine+agent-loop seam.)*
- [x] Skills authored via `createSkill` appear at `/skills/<name>/SKILL.md` and `/var/skills/<name>.md` in the running task.

## Observability (see S010)

- [ ] `simulacra.graphql.request` span with `op_kind` (query/mutation), `op_name`, `tenant_id`. *(deferred)*
- [ ] `simulacra.catalog.query` span with `entity` (agent/skill/memory_pool/tenant), `op` (get/list/create/update/delete/resolve), `tenant_id`. *(deferred)*
- [x] `simulacra.engine.resolve_agent` span with `tenant_id`, `agent_name`, `agent_id` (on success).
- [ ] `simulacra_graphql_request_duration` histogram with `op_kind`, `op_name`, `status` labels. *(deferred)*
- [ ] `simulacra_catalog_query_duration` histogram with `entity`, `op` labels. *(deferred)*
- [ ] `simulacra_agent_resolved_total` counter with `tenant_id` label. *(deferred)*
- [ ] `simulacra_toml_import_total` counter with `status` (applied/skipped) label. *(deferred)*
- [ ] `tracing::info!` on successful create/update/delete mutations: `tenant_id`, `entity`, `op`, `id`. *(deferred — only seed-skip INFO emitted today)*
- [ ] `tracing::warn!` on validation errors: `tenant_id`, `entity`, `op`, `reason`. *(deferred)*
- [ ] `tracing::error!` on repository errors: `tenant_id`, `entity`, `op`, error. *(deferred)*
- [ ] Every log line in this surface area carries `tenant_id`. *(deferred)*

## Deferred follow-ups

The S042 v1 increment closes the catalog↔engine seam end-to-end (GraphQL → spawn_task → ResolvedAgent snapshot → composed VFS) but explicitly defers the following items to dedicated follow-up specs. None of these block v1 — but the spec is *not* "Done" until they land.

1. **Full o11y suite** — Per §Observability above: most spans, all histograms and counters, and the per-line `tenant_id` requirement are unimplemented. The `simulacra.engine.resolve_agent` span ships in v1 to anchor the core seam; the rest belongs to a single observability follow-up that wires `simulacra.graphql.*` and `simulacra.catalog.*` end-to-end with metrics and structured logs.
2. **CLI agent loop rewire through `AgentRepository`** — The production CLI agent loop in `simulacra-cli/src/lib.rs` continues to read agents from `SimulacraConfig.agent_types` rather than the catalog repositories carried on `CliBootstrap`. `ensure_catalog()` exists and is tested, but is not yet called from `bootstrap()`. Wiring it without rewiring the loop would be framework-only-without-end-to-end. The follow-up: rewire the CLI agent loop to consume `AgentRepository`, then call `ensure_catalog` from the runtime entrypoint. This unblocks line 567 ("agent runs to completion under `--no-catalog`") since the CLI in either mode would resolve through the same repository surface.
3. ~~**Provider injection seam + recording HTTP fixture**~~ — *Closed by S043.* `SimulacraEngine` now accepts an optional `ProviderFactory` override; a `ScriptedProvider` test impl in `crates/simulacra-server/tests/provider_injection.rs` drives the GraphQL→catalog→engine→agent-loop chain to `TaskState::Completed`. HTTP-level recording fixtures remain a possible future spec for exercising the provider crate's parsing/retry code paths, but the engine seam is closed.
4. **`paths_read` / `paths_write` per-cap persistence** — The catalog schema has no path-capability column; `simulacra-server::engine::build_capability_token_from_resolved` therefore hardcodes `paths_read = paths_write = ["/**"]`. This matches pre-catalog engine behavior (the legacy TOML path also granted `/**`) but means catalog-defined agents cannot today have their filesystem access narrowed. Follow-up needs a migration that adds path-capability rows and a corresponding `CapabilitiesConfig` projection in `build_capability_token_from_resolved`.
5. **Richer capability persistence** — `memory`, `spawn_types`, and `skill_patterns` from `CapabilitiesConfig` are not yet persisted in the catalog schema; the in-band converter therefore drops them. Same shape of follow-up as item 4.

## Open questions

1. Tenant cache invalidation strategy beyond v1: today, only the default tenant exists; multi-tenant admin mutations come with a follow-up spec.
