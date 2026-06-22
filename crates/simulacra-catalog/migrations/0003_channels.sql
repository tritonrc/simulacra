-- S046 — Channels v1.
-- Tenant-scoped channel definitions. `kind` is one of:
--   slack | teams | email | webhook | manual
-- The `config` column is opaque JSON, kind-specific. v1 doesn't validate it.
CREATE TABLE channels (
  id          TEXT PRIMARY KEY,
  tenant_id   TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL,
  config      TEXT NOT NULL,
  created_at  TIMESTAMP NOT NULL,
  updated_at  TIMESTAMP NOT NULL,
  UNIQUE (tenant_id, name)
);
CREATE INDEX idx_channels_tenant ON channels(tenant_id);

-- Many-to-many join: an agent listens on N channels.
CREATE TABLE agent_channels (
  agent_id    TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  channel_id  TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  PRIMARY KEY (agent_id, channel_id)
);
CREATE INDEX idx_agent_channels_channel ON agent_channels(channel_id);
