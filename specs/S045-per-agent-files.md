# S045 — Per-Agent Files (Static)

**Status:** Draft
**Crates:** `simulacra-catalog`, `simulacra-graphql`, `simulacra-server`, `simulacra-vfs`

## Dependencies

- **S001** — VFS composition (`CatalogAgentFileFs` is a new layer)
- **S031** — API server (host for the new REST upload/download endpoints)
- **S036** — Task files & artifacts (`ArtifactStore` precedent — NOT reused; new `AgentFileStore` trait, see Design)
- **S042** — Agent catalog (extends `Agent` with a files association)

## Scope

Per-agent files are arbitrary binary content (PDF, image, CSV, docx, …) attached to an agent **definition**, not a task. Every task spawned from that agent sees those files in its workspace at `/var/agent_files/<name>`. They cover the **Files** section of the agent-builder form (project_northstar_agent_form memory).

**In scope (v1):**
- `agent_files` catalog table (metadata)
- `agent_file_bytes` catalog table (BLOB body, separable from metadata reads)
- `AgentFileStore` trait + `SqliteBlobAgentFileStore` impl
- REST `POST /api/v1/agents/<agent_id>/files` (multipart/form-data upload)
- REST `GET /api/v1/agents/<agent_id>/files/<file_id>/bytes` (download)
- GraphQL: `Agent.files: [AgentFile!]!`, `agentFile(id)` query, `detachAgentFile(id)` mutation
- New VFS layer `CatalogAgentFileFs` mounted at `/var/agent_files/`
- `SimulacraEngine` wires `CatalogAgentFileFs` into per-task VFS

**Out of scope (deferred):**
- **Dynamic files** (URL pull, integration source, scheduled refresh) — own follow-up spec (S046+).
- **Versioning** — replacing a file by name overwrites; no history.
- **Per-tenant/per-agent quotas** — the upload accepts what it gets in v1; quota enforcement is its own spec.
- **Cross-agent file sharing** — files are strictly per-agent.
- **Streaming uploads** — v1 buffers the multipart body in memory before write. Large files (>~100MB) are explicitly out of scope and rejected (see Behavior).
- **GraphQL file-bytes inline** — bytes are NEVER returned through GraphQL; only metadata.
- **File transformations / OCR / text extraction** — that's the agent's job at runtime.
- **Multi-level filenames** (`docs/handbook.pdf`) — names are flat, validated against `[A-Za-z0-9 ._-]+`. Subdirectories are a future spec if the UI grows folder UX.

## Context

### Why a new `AgentFileStore` (not reusing `ArtifactStore`)

`ArtifactStore::put(tenant, task_id, path, data)` from S036 is keyed by `task_id` — its on-disk layout is `<root>/<tenant>/<task_id>/<path>`. Agent files don't have a task scope; the natural key is `(tenant, agent_id, file_id)`. Reusing `ArtifactStore` would force a fake `task_id` per agent, which inverts the abstraction. A small, parallel `AgentFileStore` trait keeps S036 unchanged and lets the storage backend evolve independently.

### v1 storage choice: SQLite BLOB

The default `AgentFileStore` impl (`SqliteBlobAgentFileStore`) stores bytes in a dedicated `agent_file_bytes` table inside the existing catalog DB, separable from metadata so a `SELECT * FROM agent_files` doesn't pull blobs. SQLite handles blobs up to several hundred MB; for static handbooks / templates / CSVs this is fine. Filesystem and S3 backends are future impls behind the same trait.

## Design

### Catalog schema

```sql
-- New migration 0002_agent_files.sql

CREATE TABLE agent_files (
  id          TEXT PRIMARY KEY,                    -- ULID
  agent_id    TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,                       -- flat filename, e.g. "handbook.pdf"
  mime_type   TEXT NOT NULL,                       -- e.g. "application/pdf"
  size_bytes  INTEGER NOT NULL,                    -- denormalised for fast list views
  created_at  TIMESTAMP NOT NULL,
  updated_at  TIMESTAMP NOT NULL,
  UNIQUE (agent_id, name)
);

CREATE INDEX idx_agent_files_agent ON agent_files(agent_id);

CREATE TABLE agent_file_bytes (
  file_id     TEXT PRIMARY KEY REFERENCES agent_files(id) ON DELETE CASCADE,
  bytes       BLOB NOT NULL
);
```

Tenant scoping is enforced via `agent_id`'s FK to `agents` (which carries `tenant_id`); every repo method takes `&TenantId` first and validates the agent belongs to the tenant before touching files.

### Models

```rust
// crates/simulacra-catalog/src/models.rs (additions)
pub struct AgentFile {
    pub id: AgentFileId,
    pub agent_id: AgentId,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct NewAgentFile<'a> {
    pub agent_id: &'a AgentId,
    pub name: &'a str,
    pub mime_type: &'a str,
    pub bytes: &'a [u8],
}
```

### Repo trait

```rust
// crates/simulacra-catalog/src/repo/mod.rs (additions)
#[async_trait]
pub trait AgentFileRepository: Send + Sync {
    async fn create(&self, tenant: &TenantId, input: NewAgentFile<'_>) -> Result<AgentFile, CatalogError>;
    async fn get(&self, tenant: &TenantId, id: &AgentFileId) -> Result<AgentFile, CatalogError>;
    async fn list_for_agent(&self, tenant: &TenantId, agent: &AgentId) -> Result<Vec<AgentFile>, CatalogError>;
    async fn read_bytes(&self, tenant: &TenantId, id: &AgentFileId) -> Result<Vec<u8>, CatalogError>;
    async fn delete(&self, tenant: &TenantId, id: &AgentFileId) -> Result<(), CatalogError>;
}
```

Failure modes return `CatalogError::NotFound`, `Conflict` (duplicate name), or `Validation` (bad mime type / oversize / bad name).

### `AgentFileStore` trait

The repo's `read_bytes` / blob writes go through a small storage trait so the SQLite default can be swapped:

```rust
// crates/simulacra-catalog/src/agent_file_store.rs (new)
#[async_trait]
pub trait AgentFileStore: Send + Sync {
    async fn put(&self, file_id: &AgentFileId, bytes: &[u8]) -> Result<(), CatalogError>;
    async fn get(&self, file_id: &AgentFileId) -> Result<Vec<u8>, CatalogError>;
    async fn delete(&self, file_id: &AgentFileId) -> Result<(), CatalogError>;
}

pub struct SqliteBlobAgentFileStore { /* shares the catalog connection */ }
```

`SqliteAgentFileRepository::create/read_bytes/delete` delegate the byte half to `Arc<dyn AgentFileStore>`. v2 backends (filesystem, S3) implement only the trait without touching the repo.

### REST endpoints (added to `simulacra-server` API)

```
POST /api/v1/agents/<agent_id>/files
  Auth: existing API auth (S031)
  Content-Type: multipart/form-data
  Form fields: file (binary, with filename + content-type)
  Response: 201 Created
    Body: AgentFile JSON (id, agent_id, name, mime_type, size_bytes, created_at, updated_at)
  Errors:
    404 — agent_id not in tenant
    409 — name already taken on this agent
    413 — payload exceeds MAX_AGENT_FILE_BYTES (default 50MB, configurable)
    415 — missing/unsupported content-type on the multipart part

GET /api/v1/agents/<agent_id>/files/<file_id>/bytes
  Auth: existing API auth
  Response: 200 OK, Content-Type: <mime_type>, Content-Length: <size_bytes>
  Errors:
    404 — file not in agent (or agent not in tenant)

DELETE /api/v1/agents/<agent_id>/files/<file_id>
  Optional convenience; redundant with GraphQL detachAgentFile. Decision deferred — implement only if the UI needs it without GraphQL.
```

### GraphQL surface

```graphql
type AgentFile {
  id: ID!
  agentId: ID!
  name: String!
  mimeType: String!
  sizeBytes: Int!     # i64 in Rust → GraphQL Int (caps at 2^31; >2GB files OOS)
  createdAt: DateTime!
  updatedAt: DateTime!
  "Pre-signed URL or relative path the UI uses to fetch bytes via REST."
  downloadUrl: String!
}

extend type Agent {
  files: [AgentFile!]!
}

extend type Query {
  agentFile(id: ID!): AgentFile
}

extend type Mutation {
  detachAgentFile(id: ID!): Boolean!
}
```

`downloadUrl` is the REST path (`/api/v1/agents/<agent_id>/files/<file_id>/bytes`). The UI needs auth on the request; the server returns a relative URL and lets the client attach the existing API key.

No `attachAgentFile` mutation — uploads go through the REST endpoint.

### VFS layer: `CatalogAgentFileFs`

```rust
// crates/simulacra-vfs/src/catalog_agent_file_fs.rs (new)
pub struct CatalogAgentFileFs {
    files: Vec<AgentFile>,                      // snapshot at task spawn
    bytes: Arc<dyn AgentFileStore>,              // for lazy reads
}

impl FsLayer for CatalogAgentFileFs {
    fn list_dir("/") -> Vec<entry per file>
    fn read("/<name>") -> bytes via AgentFileStore::get(file.id)
    fn write -> Errno::ROFS
    fn remove -> Errno::ROFS
}
```

Mounted by `SimulacraEngine::spawn_task` at `/var/agent_files/` from the resolved agent's files snapshot — same pattern as `CatalogSkillFs` at `/var/skills/`.

### `ResolvedAgent` extension

`simulacra_catalog::ResolvedAgent` gains `files: Vec<AgentFile>`. `AgentRepository::resolve` joins them into the resolved snapshot frozen at spawn time (consistent with S042 assertion 6 — catalog mutations don't affect a running task).

## Behavior

### Upload
- Multipart form with one part named `file`. Filename and content-type come from the part headers.
- Filename validated against `[A-Za-z0-9 ._-]+`, max 255 bytes. Empty / invalid → 400.
- Mime type required and stored as-is. v1 doesn't validate the byte content matches the declared type.
- Size limit enforced before any blob write (`MAX_AGENT_FILE_BYTES`, default 50MB). Over limit → 413.
- Duplicate name on same agent → 409. The UI must `detachAgentFile` before uploading a new version.

### Download
- Returns raw bytes with `Content-Type: <mime_type>` and `Content-Length`. No `Content-Disposition` — the agent VFS reader doesn't need it; the UI can derive a download disposition client-side.
- Bytes load fully into memory in v1 (no streaming). >50MB never reaches this path because upload rejects above that.

### Per-task VFS
- Mounted at `/var/agent_files/` regardless of whether the agent has any files (empty dir is fine).
- The mount is a **snapshot at spawn time**. Files added/removed after spawn don't affect the running task. Mirrors S042 assertion 6.
- `read("/var/agent_files/<name>")` returns bytes verbatim (no transcoding, no MIME-aware processing).
- Writes / deletes return `Errno::ROFS`.

### Detach
- `detachAgentFile(id)` deletes the catalog row + blob row in one transaction.
- Tasks already running (which hold a snapshot) keep their copy.
- Detaching a file that doesn't exist returns `false` (not an error).

## Assertions

### Catalog
- [ ] Migration 0002 creates `agent_files` and `agent_file_bytes` tables and the `agent_files(agent_id)` index.
- [ ] `AgentFileRepository::create` populates id + timestamps and stores bytes via `AgentFileStore`.
- [ ] `create` with duplicate name on same agent returns `Conflict`.
- [ ] `create` with name not matching `[A-Za-z0-9 ._-]+` returns `Validation`.
- [ ] `list_for_agent` returns all files for an agent in `created_at, id` order.
- [ ] `read_bytes` returns the original bytes verbatim.
- [ ] `delete` cascades to `agent_file_bytes`.
- [ ] Tenant A cannot read/list/delete tenant B's agent files via any repo method.
- [ ] `AgentRepository::resolve` carries `Vec<AgentFile>` on `ResolvedAgent`.

### REST upload
- [ ] `POST /agents/<id>/files` with valid multipart returns 201 and the AgentFile JSON.
- [ ] Upload to a missing agent_id (in this tenant) returns 404.
- [ ] Upload to an agent owned by another tenant returns 404 (NOT 403 — don't leak existence).
- [ ] Duplicate name returns 409.
- [ ] Body exceeding `MAX_AGENT_FILE_BYTES` returns 413 (and stores nothing).
- [ ] Bad filename returns 400.

### REST download
- [ ] Known file returns 200 + `Content-Type: <mime>` + bytes.
- [ ] Unknown file_id returns 404.
- [ ] Cross-tenant access returns 404.

### GraphQL
- [ ] `Agent.files` returns the agent's files in `created_at, id` order.
- [ ] `Agent.files` returns empty list for an agent with no files.
- [ ] `agentFile(id)` returns metadata for a known id.
- [ ] `agentFile(id)` returns null for unknown id and for cross-tenant id.
- [ ] `detachAgentFile(id)` deletes the file and returns `true`.
- [ ] `detachAgentFile` of unknown id returns `false`.
- [ ] `detachAgentFile` of cross-tenant id returns `false` (does NOT error, does NOT leak existence).

### VFS
- [ ] `list_dir("/var/agent_files/")` lists every file's name as an entry.
- [ ] `read("/var/agent_files/<name>")` returns bytes verbatim.
- [ ] `read` of unknown name returns `Errno::NOENT`.
- [ ] `write` returns `Errno::ROFS`.
- [ ] `remove` returns `Errno::ROFS`.
- [ ] When an agent has zero files, `/var/agent_files/` exists (empty dir).
- [ ] Snapshot semantics: a file detached after spawn stays readable from the running task's VFS.

### E2E (Phase 3a)
- [ ] Upload via REST → `Agent.files` GraphQL returns it → spawn_task → `/var/agent_files/<name>` is readable from the per-task composed VFS with original bytes.
- [ ] Detach via GraphQL → next spawn_task does NOT mount the file.

## Observability

- [ ] `simulacra.catalog.agent_file.create` span: `tenant_id`, `agent_id`, `mime_type`, `size_bytes`.
- [ ] `simulacra.catalog.agent_file.delete` span: `tenant_id`, `agent_id`, `file_id`.
- [ ] `simulacra.api.agent_file.upload` span: `tenant_id`, `agent_id`, `outcome` (created/conflict/oversize/etc).
- [ ] `simulacra.api.agent_file.download` span: `tenant_id`, `agent_id`, `file_id`, `bytes`.
- [ ] `tracing::warn!` on upload validation failures: `tenant_id`, `agent_id`, `reason`.

## Open questions

1. Should the upload endpoint accept multiple files in one request? Deferred — UI can issue N requests. Multi-part-with-many-files is a v2 ergonomic.
2. Pre-signed download URLs (instead of API-key-on-every-request)? Deferred until the UI grows a non-cookie-auth flow.
3. `AgentFile.downloadUrl` absolute vs relative? Relative for v1 (`/api/v1/...`); absolute requires knowing the public host, which depends on deployment.
