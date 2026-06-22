# S036 — Task File Attachments & Artifact Retrieval

**Status:** Active
**Depends on:** S031 (API server), S034 (SimulacraEngine), S033 (integration fabric)

## Problem

Simulacra proves the plumbing works — API → engine → agent → credential injection → completion. But the enterprise value loop requires **file in, reasoning, file out:**

- A user provides a spreadsheet of expenses → agent categorizes and flags anomalies → user downloads the report
- An agent pulls deal data from HubSpot → analyzes pipeline health → produces a formatted summary
- A webhook delivers a customer complaint → agent enriches from CRM → writes an incident report

Today, the only way to seed an agent's workspace is the task description string. There is no way for clients to retrieve what the agent produced. The full enterprise task lifecycle is incomplete.

## Design

### File attachments on task creation

`POST /api/v1/tasks/create` accepts an optional `files` map. Each entry is seeded into `/workspace/` before the agent loop starts.

```json
{
  "task": "Categorize these Q1 expenses and flag anything over $10k",
  "files": {
    "expenses.csv": { "data": "vendor,amount,category,date\nAcme Corp,12500,Software,2026-01-15\n..." },
    "logo.png": { "data": "iVBORw0KGgo...", "encoding": "base64" }
  }
}
```

Each file entry is an object with:
- `data` (required): the file content as a string
- `encoding` (optional): `"utf8"` (default) or `"base64"`. Explicit encoding avoids ambiguity — no magic prefix sniffing.

Files are written to `/workspace/{filename}` alongside the existing `task.md`. The agent sees them via `file_read` or `list_dir /workspace/`.

**Validation happens synchronously in the HTTP handler, before task creation:**
- Filenames are validated (no `..`, no absolute paths, no empty, no path separators yielding escape). Invalid names return 400.
- Size limits are checked against **decoded** byte length. Individual file: 10 MB. Total: 50 MB. Exceeded limits return 413.
- Filenames must be relative paths within `/workspace/`. Nested paths like `reports/q1.csv` are permitted. The engine creates parent directories.

Only after validation passes does the handler call `spawn_task`. This ensures bad input produces synchronous HTTP errors, not async task failures.

### Artifact retrieval

Agents write output to `/proc/mailbox/`. The API exposes these artifacts.

**List artifacts:**
```
GET /api/v1/tasks/{task_id}/artifacts
Authorization: ApiKey demo-key

200 OK
{
  "ok": true,
  "data": {
    "artifacts": [
      { "path": "summary.md", "size": 2847, "content_type": "text/markdown" },
      { "path": "reports/flagged.csv", "size": 1203, "content_type": "text/csv" }
    ]
  }
}
```

`list` is recursive — returns all artifacts under the task, with paths relative to the mailbox root. Directories are not represented as separate entries; only files appear.

**Retrieve single artifact:**
```
GET /api/v1/tasks/{task_id}/artifacts/{path}
Authorization: ApiKey demo-key

200 OK
Content-Type: text/markdown  (inferred from extension)
Content-Disposition: inline; filename="summary.md"
Body: <raw file bytes>
```

Single-artifact download returns **raw bytes**, not a JSON envelope. This is an intentional exception to the S031 JSON envelope convention — file download endpoints must return the file itself to be useful to clients. The `list` endpoint uses the standard JSON envelope. Error responses (401, 403, 404) still use the JSON envelope.

**Ownership:** artifact routes enforce the same tenant ownership check as `task_status`. Only the tenant that created the task can retrieve its artifacts.

**Timing:** artifacts are available as soon as the agent writes them — clients can poll during execution, not only after completion. This supports streaming artifact patterns (agent writes partial results as it works).

### Artifact store

Artifacts are persisted to durable storage at write time — not held in memory.

```rust
/// Durable artifact storage, keyed by (task_id, path).
trait ArtifactStore: Send + Sync {
    /// Persist an artifact. Overwrites if path exists. Atomic: readers see
    /// either the old content or the new content, never partial.
    fn put(&self, task_id: &str, tenant: &str, path: &str, data: &[u8]) -> Result<(), ArtifactError>;

    /// Retrieve artifact bytes. Returns ArtifactError::NotFound if missing.
    fn get(&self, task_id: &str, path: &str) -> Result<Vec<u8>, ArtifactError>;

    /// List all artifacts for a task. Recursive. Returns relative paths + sizes.
    fn list(&self, task_id: &str) -> Result<Vec<ArtifactEntry>, ArtifactError>;

    /// Delete all artifacts for a task (cleanup/retention).
    fn delete_task(&self, task_id: &str) -> Result<(), ArtifactError>;
}

/// Metadata for a single artifact.
struct ArtifactEntry {
    /// Relative path within the task's artifact namespace (e.g. "summary.md", "reports/q1.csv").
    path: String,
    /// Size in bytes.
    size: u64,
}
```

**Tenant isolation:** `put` takes `tenant` for attribution and future per-tenant retention. The local disk layout is `{root}/{tenant}/{task_id}/{path}`. Tasks cannot access artifacts from other tasks — the store is always scoped by task_id.

**Atomicity:** `put` on local disk writes to a temp file and renames (atomic on POSIX). Readers always see a complete file or the previous version, never a partial write.

**Path safety:** The store validates `path` before joining with the filesystem root. Paths containing `..`, absolute paths, or null bytes are rejected with `ArtifactError::InvalidPath`. This is defense-in-depth — the VFS layer also normalizes paths, but the store does not trust its caller.

**Backends:**
- **`LocalDiskArtifactStore`** — writes to `{artifacts_dir}/{tenant}/{task_id}/{path}`. Default for dev and self-hosted.
- **`S3ArtifactStore`** — interface defined here, implementation is a future spec.

Configuration:
```toml
[artifacts]
backend = "local"                   # or "s3" (future)
dir = "/var/simulacra/artifacts"        # root directory for local backend
retention_days = 30                 # default retention, overridable per tenant
```

### VFS integration: MailboxFs

`MailboxFs` is a VFS layer in the composition stack. It implements the full `VirtualFs` trait (not just read/write — also `list_dir`, `exists`, `metadata`, `mkdir`, `remove`).

**Stack position:**
```
ProcFs          (intercepts /proc/**, delegates /proc/mailbox/** to inner)
  └─ ServiceFs  (intercepts /svc/**)
      └─ MailboxFs  (intercepts /proc/mailbox/**)
          └─ MemoryFs  (workspace, task.md, attached files)
```

ProcFs already delegates `/proc/mailbox/**` to its inner VFS. MailboxFs sits inside ProcFs and intercepts those delegated calls. For all non-mailbox paths, MailboxFs passes through to MemoryFs.

MailboxFs operations:
- `write("/proc/mailbox/x", data)` → `artifact_store.put(task_id, tenant, "x", data)` + delegate to inner MemoryFs (agent can re-read within session)
- `read("/proc/mailbox/x")` → `artifact_store.get(task_id, "x")` (authoritative source, not memory)
- `list_dir("/proc/mailbox")` → `artifact_store.list(task_id)` mapped to VFS entries
- `exists("/proc/mailbox/x")` → `artifact_store.get(task_id, "x").is_ok()`
- `mkdir("/proc/mailbox/subdir")` → no-op success (directories are implicit in the store)
- `remove("/proc/mailbox/x")` → not supported (artifacts are immutable once written; overwrite via `put` is allowed)
- All other paths → delegate to inner VFS unchanged

MailboxFs holds: `task_id: String`, `tenant: String`, `store: Arc<dyn ArtifactStore>`, `inner: V`.

## Assertions

### File attachment seeding

- [x] `POST /api/v1/tasks/create` accepts optional `files: HashMap<String, FileAttachment>` in the request body
- [x] `FileAttachment` has `data: String` (required) and `encoding: Option<String>` (`"utf8"` default, `"base64"`)
- [x] Each file in `files` is written to `/workspace/{filename}` before the agent loop starts
- [x] Validation (filenames, sizes) happens synchronously in the HTTP handler before task creation
- [x] Agent can read attached files via `file_read` and `list_dir /workspace/`
- [x] File attachments exceeding 10 MB (decoded bytes) per file return 413
- [x] Total attachments exceeding 50 MB (decoded bytes) return 413
- [x] Filenames containing `..` or absolute paths are rejected with 400
- [x] Nested filenames like `reports/q1.csv` are permitted and parent dirs are created
- [x] Empty `files` map or omitted `files` field behaves identically to current behavior (just `task.md`)

### Artifact retrieval API

- [x] `GET /api/v1/tasks/{task_id}/artifacts` returns JSON envelope with list of artifacts (path, size, content_type)
- [x] `GET /api/v1/tasks/{task_id}/artifacts/{path}` returns raw file bytes with Content-Type and Content-Disposition headers
- [x] Error responses (401, 403, 404) from artifact routes use the standard JSON envelope
- [x] Artifact routes enforce tenant ownership (same check as `task_status`)
- [x] Unauthenticated requests return 401
- [x] Requests for tasks owned by a different tenant return 403
- [x] Requests for non-existent tasks return 404
- [x] Requests for non-existent artifact paths return 404
- [x] Artifacts are available during task execution (not only after completion)
- [x] Nested artifact paths work: `GET .../artifacts/reports/q1-summary.md`

### Artifact store

- [x] `ArtifactStore` trait is defined with `put`, `get`, `list`, `delete_task` methods
- [x] `ArtifactEntry` struct is defined with `path: String` and `size: u64`
- [x] `put` includes `tenant` parameter for isolation and retention scoping
- [x] `LocalDiskArtifactStore` writes to `{dir}/{tenant}/{task_id}/{path}`
- [x] `put` is atomic on local disk (write-to-temp + rename)
- [x] Store validates paths: rejects `..`, absolute paths, null bytes with `ArtifactError::InvalidPath`
- [x] `list` is recursive — returns all files under the task, not just top-level
- [x] `delete_task` removes all artifacts for a given task
- [x] `S3ArtifactStore` trait bound is defined (interface only, not implemented in this spec)

### MailboxFs VFS layer

- [x] `MailboxFs` implements the full `VirtualFs` trait
- [x] Stack order: `ProcFs → ServiceFs → MailboxFs → MemoryFs`
- [x] `write` to `/proc/mailbox/**` persists to artifact store AND delegates to inner VFS
- [x] `read` from `/proc/mailbox/**` reads from artifact store (authoritative)
- [x] `list_dir("/proc/mailbox")` returns entries from artifact store
- [x] `exists` for mailbox paths checks artifact store
- [x] `mkdir` on mailbox paths is a no-op success
- [x] `remove` on mailbox paths returns `PermissionDenied` (artifacts are append/overwrite only)
- [x] Non-mailbox paths pass through to inner VFS unchanged
- [x] Artifacts survive task completion and VFS drop (stored durably, not in memory)
- [x] Artifact retrieval via API works after the agent's VFS has been dropped

### E2E: Enterprise task lifecycle

- [x] Toy SaaS serves realistic business data (deals with amounts, stages, close dates, owners — at least 20 rows)
- [x] Agent discovers integration via `/svc/toy-saas/` (reads config.json to learn base URL)
- [x] Agent fetches deal data from toy SaaS (credential injection, no hardcoded auth)
- [x] Agent writes a report artifact to `/proc/mailbox/`
- [x] Client retrieves the artifact via `GET /api/v1/tasks/{id}/artifacts/{path}` and verifies non-empty content
- [x] End-to-end test proves: task create → agent execution → integration discovery → data fetch → artifact write → artifact retrieval

### E2E: File attachment round-trip

- [x] Client creates task with attached CSV file via `files` field
- [x] Agent reads the attached file from `/workspace/`
- [x] Agent writes results to `/proc/mailbox/`
- [x] Client retrieves the output artifact
- [x] End-to-end test proves: file in → processing → file out

## Implementation notes

### Engine changes

`spawn_task` gains an optional `files: Option<HashMap<String, FileAttachment>>` parameter. Validation (path safety, size limits) is performed by the caller (HTTP handler) before invoking `spawn_task`. The engine seeds files into MemoryFs before starting the agent loop:

```rust
// Inside the worker, after MemoryFs construction:
if let Some(files) = files {
    for (name, attachment) in &files {
        let path = format!("/workspace/{name}");
        // Parent dirs created by MemoryFs.write (or explicit mkdir)
        inner_vfs.write(&path, &attachment.decode()?)?;
    }
}
```

### Artifact store wiring

The `SimulacraEngine` holds an `Arc<dyn ArtifactStore>`, constructed at startup from config. Each `spawn_task` passes `task_id`, `tenant`, and the store reference into the `MailboxFs` layer.

Artifact API routes are added to `build_router` and read directly from the `ArtifactStore` in `AppState` — no VFS reference needed.

### API schema update

The self-describing schema endpoint (`GET /api/v1/schema`) must be updated to include the new `files` field on `CreateTaskRequest` and the two new artifact routes. This maintains the S031 contract that the schema is always current.

### Toy SaaS enrichment

The toy SaaS gains:
- `GET /api/deals` — returns 20-30 deals with: id, name, amount, stage (discovery/proposal/negotiation/closed_won/closed_lost), close_date, owner, last_activity_date
- `GET /api/contacts` — returns contacts with: id, name, email, company, deal_ids
- `GET /api/pipeline/summary` — returns aggregate pipeline stats (total value, stage breakdown, at-risk count)

Data is deterministic (hardcoded, not random) so E2E assertions are stable.

## Out of scope

- S3 artifact backend implementation — interface defined here, implementation is a future spec
- Streaming file uploads (multipart) — future; JSON with explicit encoding is sufficient for proving the loop
- File format conversion (PDF generation, etc.) — capability layer concern, not platform
- Large file handling (>50 MB) — enterprise tier concern
- Provider injection seam for deterministic E2E tests — E2E tests in this spec use real LLM calls; mock-based CI tests are a separate concern
