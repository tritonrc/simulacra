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
