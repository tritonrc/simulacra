-- S045 — Per-agent files (static).
--
-- Files are arbitrary bytes attached to an agent definition. Metadata
-- and bytes live in two tables so a list view (which only needs
-- metadata) doesn't pull blob payloads.

CREATE TABLE agent_files (
  id          TEXT PRIMARY KEY,                                    -- ULID
  agent_id    TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,                                       -- flat filename, e.g. "handbook.pdf"
  mime_type   TEXT NOT NULL,                                       -- e.g. "application/pdf"
  size_bytes  INTEGER NOT NULL,                                    -- denormalised for fast list views
  created_at  TIMESTAMP NOT NULL,
  updated_at  TIMESTAMP NOT NULL,
  UNIQUE (agent_id, name)
);

CREATE TABLE agent_file_bytes (
  file_id     TEXT PRIMARY KEY REFERENCES agent_files(id) ON DELETE CASCADE,
  bytes       BLOB NOT NULL
);

CREATE INDEX idx_agent_files_agent ON agent_files(agent_id);
