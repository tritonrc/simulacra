# S042 Agent Catalog & GraphQL Control Plane — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> This plan also obeys the protocol in `CLAUDE.md`: Phase 1 (red tests via copilot GPT-5.4 + sub-agent review/reconcile), Phase 2 (green via sub-agents), Phase 3 (e2e + mechanical), Phase 4 (review by copilot GPT-5.4 + Claude sub-agent), Phase 5 (commit). Each task below maps cleanly to a Phase 2 sub-agent dispatch; the per-step TDD discipline is what those sub-agents follow.

**Goal:** Customer-managed agent catalog (SQLite) + GraphQL control-plane API, with `SimulacraEngine` rewired to read from the catalog at task spawn and a `--no-catalog` CLI escape that runs without any DB.

**Architecture:** Two new crates (`simulacra-catalog`, `simulacra-graphql`) plus targeted edits to `simulacra-server` and `simulacra-cli`. Repository traits abstract storage so the same `SimulacraEngine` works against SQLite (default) or in-memory TOML-sourced fakes (`--no-catalog`). DB-authored skills are exposed to the agent loop via a new `CatalogSkillFs` VFS layer that mirrors S040's pattern.

**Tech Stack:** Rust, rusqlite (bundled), `tokio::task::spawn_blocking` for async wrapping, async-graphql, axum (existing), ulid, serde_json.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/simulacra-catalog/Cargo.toml` | Create | Crate manifest |
| `crates/simulacra-catalog/src/lib.rs` | Create | Public API: re-exports, `Catalog` open/migrate |
| `crates/simulacra-catalog/src/error.rs` | Create | `CatalogError` enum |
| `crates/simulacra-catalog/src/ids.rs` | Create | `TenantId`, `AgentId`, `SkillId`, `MemoryPoolId` newtypes; ULID generation |
| `crates/simulacra-catalog/src/models.rs` | Create | `Tenant`, `Agent`, `Skill`, `MemoryPool`, `ResolvedAgent`, `Page<T>`, `PageRequest` |
| `crates/simulacra-catalog/src/repo/mod.rs` | Create | Repository traits |
| `crates/simulacra-catalog/src/repo/sqlite/mod.rs` | Create | SQLite impl module root |
| `crates/simulacra-catalog/src/repo/sqlite/tenant.rs` | Create | `SqliteTenantRepository` |
| `crates/simulacra-catalog/src/repo/sqlite/agent.rs` | Create | `SqliteAgentRepository` (incl. `resolve`) |
| `crates/simulacra-catalog/src/repo/sqlite/skill.rs` | Create | `SqliteSkillRepository` |
| `crates/simulacra-catalog/src/repo/sqlite/memory_pool.rs` | Create | `SqliteMemoryPoolRepository` |
| `crates/simulacra-catalog/src/repo/memory/mod.rs` | Create | In-memory impl module root |
| `crates/simulacra-catalog/src/repo/memory/agent.rs` | Create | `MemoryAgentRepository` (read-only) |
| `crates/simulacra-catalog/src/repo/memory/skill.rs` | Create | `MemorySkillRepository` (read-only) |
| `crates/simulacra-catalog/src/repo/memory/memory_pool.rs` | Create | `MemoryMemoryPoolRepository` (read-only) |
| `crates/simulacra-catalog/src/repo/memory/tenant.rs` | Create | `MemoryTenantRepository` (single default tenant) |
| `crates/simulacra-catalog/src/migrate.rs` | Create | Migration runner |
| `crates/simulacra-catalog/migrations/0001_initial.sql` | Create | Initial schema |
| `crates/simulacra-catalog/src/skill_fs.rs` | Create | `CatalogSkillFs` (FsLayer impl) |
| `crates/simulacra-catalog/src/metrics.rs` | Create | OTel meters for catalog + skill_fs |
| `crates/simulacra-catalog/tests/migrations.rs` | Create | Migration apply/idempotent tests |
| `crates/simulacra-catalog/tests/sqlite_repos.rs` | Create | CRUD + tenant isolation tests |
| `crates/simulacra-catalog/tests/memory_repos.rs` | Create | In-memory repo tests |
| `crates/simulacra-catalog/tests/skill_fs.rs` | Create | CatalogSkillFs FsLayer tests |
| `crates/simulacra-graphql/Cargo.toml` | Create | Crate manifest |
| `crates/simulacra-graphql/src/lib.rs` | Create | Public API: schema builder, axum handler |
| `crates/simulacra-graphql/src/context.rs` | Create | `GraphQLContext`, tenant cache |
| `crates/simulacra-graphql/src/schema/mod.rs` | Create | Schema construction |
| `crates/simulacra-graphql/src/schema/agent.rs` | Create | `Agent` GraphQL type + queries + mutations |
| `crates/simulacra-graphql/src/schema/skill.rs` | Create | `Skill` GraphQL type + queries + mutations |
| `crates/simulacra-graphql/src/schema/memory_pool.rs` | Create | `MemoryPool` GraphQL type + queries + mutations |
| `crates/simulacra-graphql/src/schema/scalars.rs` | Create | DateTime, JSON scalars, ID conversions |
| `crates/simulacra-graphql/src/schema/connection.rs` | Create | Relay-style Connection helpers, cursor encode/decode |
| `crates/simulacra-graphql/src/auth.rs` | Create | axum middleware: auth → tenant resolution → context injection |
| `crates/simulacra-graphql/src/error.rs` | Create | GraphQL error mapping (`code` extension) |
| `crates/simulacra-graphql/tests/queries.rs` | Create | Query resolver tests |
| `crates/simulacra-graphql/tests/mutations.rs` | Create | Mutation resolver tests |
| `crates/simulacra-graphql/tests/auth.rs` | Create | Auth + tenant scoping tests |
| `crates/simulacra-server/src/engine.rs` | Modify | `SimulacraEngine` constructor + `spawn_task` rewired |
| `crates/simulacra-server/src/server.rs` | Modify | Mount `/graphql` route, build engine with repos |
| `crates/simulacra-server/src/lib.rs` | Modify | Re-export catalog/graphql wiring |
| `crates/simulacra-server/Cargo.toml` | Modify | Add `simulacra-catalog`, `simulacra-graphql` deps |
| `crates/simulacra-server/tests/graphql_e2e.rs` | Create | End-to-end: createAgent → spawn task → run |
| `crates/simulacra-cli/src/lib.rs` | Modify | Catalog bootstrap; `--no-catalog` flag wiring |
| `crates/simulacra-cli/src/main.rs` | Modify | Argument parsing for `--no-catalog` |
| `crates/simulacra-cli/Cargo.toml` | Modify | Add `simulacra-catalog` dep |
| `crates/simulacra-cli/tests/catalog_bootstrap.rs` | Create | TOML→DB import idempotency |
| `crates/simulacra-cli/tests/no_catalog_mode.rs` | Create | `--no-catalog` resolves agents from TOML |
| `crates/simulacra-config/src/lib.rs` | Modify | Add `[catalog]` section |
| `Cargo.toml` (workspace) | Modify | Add `simulacra-catalog`, `simulacra-graphql`, `async-graphql`, `ulid` |

---

### Task 1: Scaffold `simulacra-catalog` crate with core types and migration runner

**Files:**
- Create: `crates/simulacra-catalog/Cargo.toml`
- Create: `crates/simulacra-catalog/src/lib.rs`
- Create: `crates/simulacra-catalog/src/error.rs`
- Create: `crates/simulacra-catalog/src/ids.rs`
- Create: `crates/simulacra-catalog/src/models.rs`
- Create: `crates/simulacra-catalog/src/migrate.rs`
- Create: `crates/simulacra-catalog/migrations/0001_initial.sql`
- Create: `crates/simulacra-catalog/tests/migrations.rs`
- Modify: `Cargo.toml` (workspace)

- [ ] **Step 1: Add workspace dependencies**

Modify `Cargo.toml` (workspace `[workspace.dependencies]`):

```toml
ulid = "1.1"
async-graphql = { version = "7", features = ["chrono", "playground"] }
async-graphql-axum = "7"
async-trait = "0.1"
```

Add `crates/simulacra-catalog` and `crates/simulacra-graphql` to `[workspace] members`.

- [ ] **Step 2: Create `crates/simulacra-catalog/Cargo.toml`**

```toml
[package]
name = "simulacra-catalog"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
simulacra-types.workspace = true
simulacra-vfs.workspace = true
async-trait.workspace = true
chrono = { workspace = true, features = ["serde"] }
opentelemetry.workspace = true
rusqlite.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true
ulid = { workspace = true, features = ["serde"] }

[dev-dependencies]
tempfile = "3"
tokio = { workspace = true, features = ["full"] }
tracing-subscriber.workspace = true
```

- [ ] **Step 3: Create `src/error.rs`**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("validation: {0}")]
    Validation(String),

    #[error("read-only repository: {0}")]
    ReadOnly(String),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("join: {0}")]
    Join(#[from] tokio::task::JoinError),
}
```

- [ ] **Step 4: Create `src/ids.rs`**

```rust
use serde::{Deserialize, Serialize};
use std::fmt;
use ulid::Ulid;

macro_rules! catalog_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self { Self(Ulid::new().to_string()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name { fn from(s: String) -> Self { Self(s) } }
        impl From<&str> for $name { fn from(s: &str) -> Self { Self(s.to_owned()) } }
    };
}

catalog_id!(TenantId);
catalog_id!(AgentId);
catalog_id!(SkillId);
catalog_id!(MemoryPoolId);
```

- [ ] **Step 5: Create `src/models.rs`**

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{AgentId, MemoryPoolId, SkillId, TenantId};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tenant {
    pub id: TenantId,
    pub namespace: String,
    pub display_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryPool {
    pub id: MemoryPoolId,
    pub tenant_id: TenantId,
    pub name: String,
    pub embedding_model: Option<String>,
    pub config: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Snapshot used by SimulacraEngine — frozen for the duration of one task.
#[derive(Clone, Debug)]
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
}

#[derive(Clone, Debug)]
pub struct PageRequest {
    /// Forward pagination
    pub first: Option<u32>,
    pub after: Option<String>,
    /// Backward pagination (mutually exclusive with first/after)
    pub last: Option<u32>,
    pub before: Option<String>,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self { first: Some(20), after: None, last: None, before: None }
    }
}

#[derive(Clone, Debug)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub end_cursor: Option<String>,
    pub start_cursor: Option<String>,
    pub has_next_page: bool,
    pub has_previous_page: bool,
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
}

// NewSkill, SkillPatch, NewMemoryPool, MemoryPoolPatch follow the same
// convention. Use `Option<Option<T>>` for nullable patch fields:
// outer None = "no change", outer Some(None) = "set to null".
```

- [ ] **Step 6: Create `migrations/0001_initial.sql`**

```sql
-- 0001_initial.sql — S042 catalog schema

CREATE TABLE IF NOT EXISTS tenants (
  id           TEXT PRIMARY KEY,
  namespace    TEXT NOT NULL UNIQUE,
  display_name TEXT,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS memory_pools (
  id              TEXT PRIMARY KEY,
  tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name            TEXT NOT NULL,
  embedding_model TEXT,
  config          TEXT NOT NULL,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  UNIQUE(tenant_id, name)
);

CREATE TABLE IF NOT EXISTS agents (
  id              TEXT PRIMARY KEY,
  tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name            TEXT NOT NULL,
  description     TEXT,
  system_prompt   TEXT NOT NULL,
  model           TEXT NOT NULL,
  max_turns       INTEGER NOT NULL DEFAULT 100,
  max_tokens      INTEGER,
  memory_pool_id  TEXT REFERENCES memory_pools(id) ON DELETE SET NULL,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  UNIQUE(tenant_id, name)
);

CREATE TABLE IF NOT EXISTS skills (
  id          TEXT PRIMARY KEY,
  tenant_id   TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  description TEXT,
  body        TEXT NOT NULL,
  metadata    TEXT,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  UNIQUE(tenant_id, name)
);

CREATE TABLE IF NOT EXISTS agent_skills (
  agent_id  TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  skill_id  TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
  PRIMARY KEY (agent_id, skill_id)
);

CREATE TABLE IF NOT EXISTS agent_capabilities (
  agent_id   TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  capability TEXT NOT NULL,
  PRIMARY KEY (agent_id, capability)
);

CREATE TABLE IF NOT EXISTS seeds_applied (
  source     TEXT PRIMARY KEY,
  applied_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS schema_meta (
  version INTEGER PRIMARY KEY
);

CREATE INDEX IF NOT EXISTS idx_agents_tenant       ON agents(tenant_id);
CREATE INDEX IF NOT EXISTS idx_skills_tenant       ON skills(tenant_id);
CREATE INDEX IF NOT EXISTS idx_memory_pools_tenant ON memory_pools(tenant_id);
```

- [ ] **Step 7: Create `src/migrate.rs`**

```rust
use rusqlite::Connection;

use crate::error::CatalogError;

const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
];

pub fn run(conn: &mut Connection) -> Result<(), CatalogError> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS schema_meta (version INTEGER PRIMARY KEY);",
    )?;

    let current: i32 = conn
        .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_meta", [], |r| r.get(0))?;

    let tx = conn.transaction()?;
    for (version, sql) in MIGRATIONS {
        if *version > current {
            tx.execute_batch(sql)?;
            tx.execute("INSERT INTO schema_meta (version) VALUES (?1)", [version])?;
        }
    }
    tx.commit()?;
    Ok(())
}
```

- [ ] **Step 8: Create `src/lib.rs`**

```rust
//! S042 — Agent Catalog. SQLite-backed (default) or in-memory (--no-catalog).

pub mod error;
pub mod ids;
pub mod migrate;
pub mod models;
pub mod repo;
pub mod skill_fs;
pub(crate) mod metrics;

pub use error::CatalogError;
pub use ids::{AgentId, MemoryPoolId, SkillId, TenantId};
pub use models::{Agent, AgentPatch, MemoryPool, NewAgent, Page, PageRequest, ResolvedAgent, Skill, Tenant};
pub use skill_fs::CatalogSkillFs;

use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Top-level catalog handle. Owns the connection (or pool); produces
/// repository handles via `agents()`, `skills()`, etc.
pub struct Catalog {
    conn: Arc<Mutex<Connection>>,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        migrate::run(&mut conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn open_in_memory() -> Result<Self, CatalogError> {
        let mut conn = Connection::open_in_memory()?;
        migrate::run(&mut conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub(crate) fn conn(&self) -> Arc<Mutex<Connection>> { Arc::clone(&self.conn) }
}
```

- [ ] **Step 9: Write failing test `tests/migrations.rs`**

```rust
use simulacra_catalog::Catalog;
use tempfile::TempDir;

#[test]
fn fresh_db_runs_migrations_and_creates_tables() {
    let tmp = TempDir::new().unwrap();
    let catalog = Catalog::open(&tmp.path().join("catalog.db")).unwrap();

    let conn = catalog_conn(&catalog);
    let conn = conn.lock().unwrap();

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    for expected in [
        "agent_capabilities", "agent_skills", "agents", "memory_pools",
        "schema_meta", "seeds_applied", "skills", "tenants",
    ] {
        assert!(tables.iter().any(|t| t == expected), "missing table: {expected} in {tables:?}");
    }
}

#[test]
fn reopen_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("catalog.db");

    let _c1 = Catalog::open(&path).unwrap();
    let _c2 = Catalog::open(&path).unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let max_version: i32 = conn.query_row("SELECT MAX(version) FROM schema_meta", [], |r| r.get(0)).unwrap();
    assert_eq!(max_version, 1);
}

#[test]
fn pragmas_set_per_connection() {
    let tmp = TempDir::new().unwrap();
    let catalog = Catalog::open(&tmp.path().join("catalog.db")).unwrap();
    let conn = catalog_conn(&catalog);
    let conn = conn.lock().unwrap();

    let journal: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0)).unwrap();
    assert_eq!(journal.to_lowercase(), "wal");

    let fk: i32 = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
    assert_eq!(fk, 1);
}

// Test helper — exposes the inner connection for assertions.
// Add `pub fn conn_for_tests(&self) -> Arc<Mutex<Connection>>` in lib.rs gated on cfg(test).
fn catalog_conn(c: &Catalog) -> std::sync::Arc<std::sync::Mutex<rusqlite::Connection>> {
    c.conn_for_tests()
}
```

Add to `src/lib.rs` for test access:

```rust
#[cfg(any(test, feature = "test-internals"))]
impl Catalog {
    pub fn conn_for_tests(&self) -> std::sync::Arc<std::sync::Mutex<rusqlite::Connection>> {
        std::sync::Arc::clone(&self.conn)
    }
}
```

- [ ] **Step 10: Run tests; verify they fail with "function not found" / type errors**

```bash
cargo test -p simulacra-catalog --tests
```

Expected: compilation errors until repository module is stubbed; the migration tests will pass once Steps 1–8 compile. Iterate until migration tests pass.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml crates/simulacra-catalog/
git commit -m "feat(simulacra-catalog): scaffold crate, ids, models, migrations [S042]"
```

---

### Task 2: Repository traits + SQLite Tenant + MemoryPool repositories

**Files:**
- Create: `crates/simulacra-catalog/src/repo/mod.rs`
- Create: `crates/simulacra-catalog/src/repo/sqlite/mod.rs`
- Create: `crates/simulacra-catalog/src/repo/sqlite/tenant.rs`
- Create: `crates/simulacra-catalog/src/repo/sqlite/memory_pool.rs`
- Create: `crates/simulacra-catalog/tests/sqlite_repos.rs` (initial test set)

- [ ] **Step 1: Create `src/repo/mod.rs` with traits**

```rust
use async_trait::async_trait;

use crate::error::CatalogError;
use crate::ids::{AgentId, MemoryPoolId, SkillId, TenantId};
use crate::models::*;

pub mod sqlite;
pub mod memory;

#[async_trait]
pub trait TenantRepository: Send + Sync {
    async fn get_by_namespace(&self, namespace: &str) -> Result<Tenant, CatalogError>;
    async fn get_by_id(&self, id: &TenantId) -> Result<Tenant, CatalogError>;
    async fn create(&self, namespace: &str, display_name: Option<&str>) -> Result<Tenant, CatalogError>;
    async fn get_or_create(&self, namespace: &str, display_name: Option<&str>) -> Result<Tenant, CatalogError>;
}

#[async_trait]
pub trait MemoryPoolRepository: Send + Sync {
    async fn get(&self, tenant_id: &TenantId, id: &MemoryPoolId) -> Result<MemoryPool, CatalogError>;
    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<MemoryPool, CatalogError>;
    async fn list(&self, tenant_id: &TenantId) -> Result<Vec<MemoryPool>, CatalogError>;
    async fn create(&self, tenant_id: &TenantId, input: NewMemoryPool<'_>) -> Result<MemoryPool, CatalogError>;
    async fn update(&self, tenant_id: &TenantId, id: &MemoryPoolId, input: MemoryPoolPatch<'_>) -> Result<MemoryPool, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &MemoryPoolId) -> Result<(), CatalogError>;
}

#[async_trait]
pub trait SkillRepository: Send + Sync {
    async fn get(&self, tenant_id: &TenantId, id: &SkillId) -> Result<Skill, CatalogError>;
    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Skill, CatalogError>;
    async fn list(&self, tenant_id: &TenantId, page: PageRequest) -> Result<Page<Skill>, CatalogError>;
    async fn list_for_agent(&self, tenant_id: &TenantId, agent_id: &AgentId) -> Result<Vec<Skill>, CatalogError>;
    async fn create(&self, tenant_id: &TenantId, input: NewSkill<'_>) -> Result<Skill, CatalogError>;
    async fn update(&self, tenant_id: &TenantId, id: &SkillId, input: SkillPatch<'_>) -> Result<Skill, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &SkillId) -> Result<(), CatalogError>;
}

#[async_trait]
pub trait AgentRepository: Send + Sync {
    async fn get(&self, tenant_id: &TenantId, id: &AgentId) -> Result<Agent, CatalogError>;
    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Agent, CatalogError>;
    async fn list(&self, tenant_id: &TenantId, page: PageRequest) -> Result<Page<Agent>, CatalogError>;
    async fn create(&self, tenant_id: &TenantId, input: NewAgent<'_>) -> Result<Agent, CatalogError>;
    async fn update(&self, tenant_id: &TenantId, id: &AgentId, input: AgentPatch<'_>) -> Result<Agent, CatalogError>;
    async fn delete(&self, tenant_id: &TenantId, id: &AgentId) -> Result<(), CatalogError>;
    async fn resolve(&self, tenant_id: &TenantId, name: &str) -> Result<ResolvedAgent, CatalogError>;
    async fn capabilities(&self, agent_id: &AgentId) -> Result<Vec<String>, CatalogError>;
}
```

- [ ] **Step 2: Create `src/repo/sqlite/mod.rs`**

```rust
use std::sync::{Arc, Mutex};
use rusqlite::Connection;

use crate::Catalog;

pub mod tenant;
pub mod memory_pool;
pub mod agent;
pub mod skill;

pub use tenant::SqliteTenantRepository;
pub use memory_pool::SqliteMemoryPoolRepository;
pub use agent::SqliteAgentRepository;
pub use skill::SqliteSkillRepository;

impl Catalog {
    pub fn tenants(&self) -> SqliteTenantRepository {
        SqliteTenantRepository::new(self.conn())
    }
    pub fn memory_pools(&self) -> SqliteMemoryPoolRepository {
        SqliteMemoryPoolRepository::new(self.conn())
    }
    pub fn agents(&self) -> SqliteAgentRepository {
        SqliteAgentRepository::new(self.conn())
    }
    pub fn skills(&self) -> SqliteSkillRepository {
        SqliteSkillRepository::new(self.conn())
    }
}

pub(crate) type SharedConn = Arc<Mutex<Connection>>;

/// Spawn a blocking SQL closure on the tokio blocking pool, holding the
/// connection mutex for the duration of the closure.
pub(crate) async fn blocking<F, R>(conn: SharedConn, f: F) -> Result<R, crate::CatalogError>
where
    F: FnOnce(&mut Connection) -> Result<R, crate::CatalogError> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = conn.lock().expect("catalog mutex poisoned");
        f(&mut *guard)
    })
    .await?
}
```

- [ ] **Step 3: Create `src/repo/sqlite/tenant.rs`**

```rust
use async_trait::async_trait;
use chrono::Utc;
use rusqlite::OptionalExtension;
use ulid::Ulid;

use crate::error::CatalogError;
use crate::ids::TenantId;
use crate::models::Tenant;
use crate::repo::TenantRepository;
use crate::repo::sqlite::{SharedConn, blocking};

pub struct SqliteTenantRepository { conn: SharedConn }

impl SqliteTenantRepository {
    pub fn new(conn: SharedConn) -> Self { Self { conn } }
}

fn row_to_tenant(row: &rusqlite::Row<'_>) -> rusqlite::Result<Tenant> {
    Ok(Tenant {
        id: TenantId(row.get::<_, String>(0)?),
        namespace: row.get(1)?,
        display_name: row.get(2)?,
        created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(3)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e)))?
            .with_timezone(&Utc),
        updated_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(4)?)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?
            .with_timezone(&Utc),
    })
}

#[async_trait]
impl TenantRepository for SqliteTenantRepository {
    async fn get_by_namespace(&self, namespace: &str) -> Result<Tenant, CatalogError> {
        let ns = namespace.to_owned();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                "SELECT id, namespace, display_name, created_at, updated_at FROM tenants WHERE namespace = ?1",
                [&ns],
                row_to_tenant,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("tenant ns={ns}")))
        }).await
    }

    async fn get_by_id(&self, id: &TenantId) -> Result<Tenant, CatalogError> {
        let id = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                "SELECT id, namespace, display_name, created_at, updated_at FROM tenants WHERE id = ?1",
                [&id],
                row_to_tenant,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("tenant id={id}")))
        }).await
    }

    async fn create(&self, namespace: &str, display_name: Option<&str>) -> Result<Tenant, CatalogError> {
        let id = Ulid::new().to_string();
        let now = Utc::now().to_rfc3339();
        let ns = namespace.to_owned();
        let dn = display_name.map(str::to_owned);

        blocking(self.conn.clone(), move |c| {
            c.execute(
                "INSERT INTO tenants (id, namespace, display_name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![&id, &ns, &dn, &now],
            )
            .map_err(|e| match &e {
                rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                    CatalogError::Conflict(format!("tenant namespace already exists: {ns}")),
                _ => CatalogError::Sqlite(e),
            })?;
            c.query_row(
                "SELECT id, namespace, display_name, created_at, updated_at FROM tenants WHERE id = ?1",
                [&id],
                row_to_tenant,
            ).map_err(CatalogError::from)
        }).await
    }

    async fn get_or_create(&self, namespace: &str, display_name: Option<&str>) -> Result<Tenant, CatalogError> {
        match self.get_by_namespace(namespace).await {
            Ok(t) => Ok(t),
            Err(CatalogError::NotFound(_)) => self.create(namespace, display_name).await,
            Err(e) => Err(e),
        }
    }
}
```

- [ ] **Step 4: Create `src/repo/sqlite/memory_pool.rs`**

Mirror tenant.rs structure. Key methods: `get`, `get_by_name`, `list` (no pagination — small set), `create`, `update`, `delete`. Every query filters by `tenant_id`. JSON field `config` serialized via `serde_json::to_string` on write, parsed on read.

```rust
// Skeleton (full impl follows tenant.rs pattern; see Task 3 agent.rs for the
// patch-application idiom for nullable fields)

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use ulid::Ulid;

use crate::error::CatalogError;
use crate::ids::{MemoryPoolId, TenantId};
use crate::models::{MemoryPool, NewMemoryPool, MemoryPoolPatch};
use crate::repo::MemoryPoolRepository;
use crate::repo::sqlite::{SharedConn, blocking};

pub struct SqliteMemoryPoolRepository { conn: SharedConn }

impl SqliteMemoryPoolRepository {
    pub fn new(conn: SharedConn) -> Self { Self { conn } }
}

fn row_to_pool(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryPool> {
    let config_json: String = row.get(4)?;
    Ok(MemoryPool {
        id: MemoryPoolId(row.get(0)?),
        tenant_id: TenantId(row.get(1)?),
        name: row.get(2)?,
        embedding_model: row.get(3)?,
        config: serde_json::from_str(&config_json).unwrap_or(serde_json::Value::Null),
        created_at: parse_ts(row.get::<_, String>(5)?)?,
        updated_at: parse_ts(row.get::<_, String>(6)?)?,
    })
}

fn parse_ts(s: String) -> rusqlite::Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

#[async_trait]
impl MemoryPoolRepository for SqliteMemoryPoolRepository {
    async fn get(&self, tenant_id: &TenantId, id: &MemoryPoolId) -> Result<MemoryPool, CatalogError> {
        let tid = tenant_id.0.clone();
        let pid = id.0.clone();
        blocking(self.conn.clone(), move |c| {
            c.query_row(
                "SELECT id, tenant_id, name, embedding_model, config, created_at, updated_at \
                 FROM memory_pools WHERE tenant_id = ?1 AND id = ?2",
                params![&tid, &pid],
                row_to_pool,
            )
            .optional()?
            .ok_or_else(|| CatalogError::NotFound(format!("memory_pool id={pid} tenant={tid}")))
        }).await
    }
    // get_by_name, list, create, update, delete follow the same pattern.
    // create generates ULID id, sets created_at = updated_at = now;
    // update bumps updated_at; delete returns NotFound if 0 rows changed.
}
```

- [ ] **Step 5: Write failing test `tests/sqlite_repos.rs` for tenant + memory_pool**

```rust
use simulacra_catalog::repo::{MemoryPoolRepository, TenantRepository};
use simulacra_catalog::{Catalog, CatalogError};
use simulacra_catalog::models::NewMemoryPool;
use serde_json::json;

async fn fresh() -> Catalog { Catalog::open_in_memory().unwrap() }

#[tokio::test]
async fn tenant_create_and_get_by_namespace() {
    let cat = fresh().await;
    let repo = cat.tenants();
    let t = repo.create("acme", Some("Acme Corp")).await.unwrap();
    assert_eq!(t.namespace, "acme");

    let fetched = repo.get_by_namespace("acme").await.unwrap();
    assert_eq!(fetched.id.as_str(), t.id.as_str());
}

#[tokio::test]
async fn tenant_create_duplicate_namespace_returns_conflict() {
    let cat = fresh().await;
    let repo = cat.tenants();
    repo.create("acme", None).await.unwrap();
    let err = repo.create("acme", None).await.unwrap_err();
    assert!(matches!(err, CatalogError::Conflict(_)));
}

#[tokio::test]
async fn tenant_get_or_create_is_idempotent() {
    let cat = fresh().await;
    let repo = cat.tenants();
    let a = repo.get_or_create("default", None).await.unwrap();
    let b = repo.get_or_create("default", None).await.unwrap();
    assert_eq!(a.id.as_str(), b.id.as_str());
}

#[tokio::test]
async fn memory_pool_crud_round_trip() {
    let cat = fresh().await;
    let tenant = cat.tenants().create("acme", None).await.unwrap();
    let pools = cat.memory_pools();

    let pool = pools.create(&tenant.id, NewMemoryPool {
        name: "shared",
        embedding_model: Some("local-st-mini"),
        config: &json!({"vector_dim": 384}),
    }).await.unwrap();

    let got = pools.get(&tenant.id, &pool.id).await.unwrap();
    assert_eq!(got.name, "shared");
    assert_eq!(got.config["vector_dim"], json!(384));
}

#[tokio::test]
async fn memory_pool_cross_tenant_get_returns_not_found() {
    let cat = fresh().await;
    let alice = cat.tenants().create("alice", None).await.unwrap();
    let bob   = cat.tenants().create("bob", None).await.unwrap();
    let pools = cat.memory_pools();

    let alice_pool = pools.create(&alice.id, NewMemoryPool {
        name: "p", embedding_model: None, config: &json!({}),
    }).await.unwrap();

    let err = pools.get(&bob.id, &alice_pool.id).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}
```

- [ ] **Step 6: Run tests; iterate impl until they pass**

```bash
cargo test -p simulacra-catalog --tests
```

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-catalog/
git commit -m "feat(simulacra-catalog): repository traits + sqlite tenant/memory_pool [S042]"
```

---

### Task 3: SQLite Agent + Skill repositories with `resolve`

**Files:**
- Create: `crates/simulacra-catalog/src/repo/sqlite/agent.rs`
- Create: `crates/simulacra-catalog/src/repo/sqlite/skill.rs`
- Modify: `crates/simulacra-catalog/tests/sqlite_repos.rs` (add agent + skill tests)

- [ ] **Step 1: Create `src/repo/sqlite/skill.rs`**

Mirror memory_pool.rs. Key additional method: `list_for_agent(tenant_id, agent_id)` joins `agent_skills` and filters by both. Pagination uses `(created_at, id)` cursor encoded as base64.

Pagination helper (used by both skill and agent list):

```rust
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD as B64};

pub(crate) fn encode_cursor(created_at: &str, id: &str) -> String {
    B64.encode(format!("{created_at}|{id}"))
}

pub(crate) fn decode_cursor(cursor: &str) -> Result<(String, String), CatalogError> {
    let bytes = B64.decode(cursor)
        .map_err(|_| CatalogError::Validation("invalid cursor".into()))?;
    let s = String::from_utf8(bytes)
        .map_err(|_| CatalogError::Validation("invalid cursor".into()))?;
    let mut parts = s.splitn(2, '|');
    let ts = parts.next()
        .ok_or_else(|| CatalogError::Validation("invalid cursor".into()))?
        .to_owned();
    let id = parts.next()
        .ok_or_else(|| CatalogError::Validation("invalid cursor".into()))?
        .to_owned();
    Ok((ts, id))
}
```

`list` query with forward pagination:

```sql
SELECT ... FROM skills
WHERE tenant_id = ?1
  AND (?2 IS NULL OR (created_at, id) > (?2, ?3))
ORDER BY created_at ASC, id ASC
LIMIT ?4
```

Add `base64 = "0.22"` to `Cargo.toml`.

- [ ] **Step 2: Create `src/repo/sqlite/agent.rs`**

Implements `AgentRepository`. Key methods:

- `create`: in a transaction, insert `agents` row, then bulk-insert `agent_skills` and `agent_capabilities` if provided.
- `update`: patch with `Option<Option<T>>` semantics for nullable fields. If `skill_ids` is `Some(_)`, transactionally delete all `agent_skills` for the agent and insert the new set. Same for `capabilities`.
- `delete`: hard delete; FK cascade handles join tables.
- `resolve(tenant_id, name)`: in a single transaction:
  1. Fetch agent row by `(tenant_id, name)`.
  2. Fetch joined skills via `agent_skills`.
  3. Fetch capabilities from `agent_capabilities`.
  4. Fetch memory_pool if `memory_pool_id` is set.
  5. Return `ResolvedAgent`.
- `capabilities(agent_id)`: separate accessor used by GraphQL Agent type resolver.

```rust
async fn resolve(&self, tenant_id: &TenantId, name: &str) -> Result<ResolvedAgent, CatalogError> {
    let tid = tenant_id.0.clone();
    let name = name.to_owned();
    blocking(self.conn.clone(), move |c| {
        let tx = c.transaction()?;
        let agent: Agent = tx.query_row(
            "SELECT ... FROM agents WHERE tenant_id = ?1 AND name = ?2",
            params![&tid, &name],
            row_to_agent,
        )
        .optional()?
        .ok_or_else(|| CatalogError::NotFound(format!("agent name={name} tenant={tid}")))?;

        let skills: Vec<Skill> = tx.prepare(
            "SELECT s.id, s.tenant_id, s.name, s.description, s.body, s.metadata, s.created_at, s.updated_at \
             FROM skills s JOIN agent_skills a ON a.skill_id = s.id \
             WHERE a.agent_id = ?1 AND s.tenant_id = ?2 \
             ORDER BY s.name ASC"
        )?
        .query_map(params![agent.id.as_str(), &tid], row_to_skill)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

        let capabilities: Vec<String> = tx.prepare(
            "SELECT capability FROM agent_capabilities WHERE agent_id = ?1 ORDER BY capability"
        )?
        .query_map([agent.id.as_str()], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

        let memory_pool = match &agent.memory_pool_id {
            Some(pid) => tx.query_row(
                "SELECT ... FROM memory_pools WHERE id = ?1 AND tenant_id = ?2",
                params![pid.as_str(), &tid],
                row_to_pool,
            ).optional()?,
            None => None,
        };

        Ok(ResolvedAgent {
            id: agent.id, name: agent.name, system_prompt: agent.system_prompt,
            model: agent.model, max_turns: agent.max_turns, max_tokens: agent.max_tokens,
            skills, capabilities, memory_pool,
        })
    }).await
}
```

- [ ] **Step 3: Add tests to `tests/sqlite_repos.rs`**

```rust
use simulacra_catalog::models::{NewAgent, NewSkill, AgentPatch};
use simulacra_catalog::repo::{AgentRepository, SkillRepository};

#[tokio::test]
async fn agent_create_with_skills_and_capabilities() {
    let cat = fresh().await;
    let tenant = cat.tenants().create("acme", None).await.unwrap();
    let skill = cat.skills().create(&tenant.id, NewSkill {
        name: "summarize", description: None, body: "# Summarize\n", metadata: None,
    }).await.unwrap();

    let agent = cat.agents().create(&tenant.id, NewAgent {
        name: "assistant",
        description: None,
        system_prompt: "You are a helpful assistant.",
        model: "openai/gpt-oss-120b",
        max_turns: Some(50),
        max_tokens: None,
        memory_pool_id: None,
        skill_ids: &[skill.id.clone()],
        capabilities: &["mcp:fetcher:*".to_string()],
    }).await.unwrap();

    let resolved = cat.agents().resolve(&tenant.id, "assistant").await.unwrap();
    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.skills[0].name, "summarize");
    assert_eq!(resolved.capabilities, vec!["mcp:fetcher:*"]);
    assert_eq!(resolved.id.as_str(), agent.id.as_str());
}

#[tokio::test]
async fn agent_update_replaces_skills_transactionally() {
    let cat = fresh().await;
    let t = cat.tenants().create("t", None).await.unwrap();
    let s1 = cat.skills().create(&t.id, NewSkill { name: "a", description: None, body: "a", metadata: None }).await.unwrap();
    let s2 = cat.skills().create(&t.id, NewSkill { name: "b", description: None, body: "b", metadata: None }).await.unwrap();

    let agent = cat.agents().create(&t.id, NewAgent {
        name: "x", description: None, system_prompt: "p", model: "m",
        max_turns: None, max_tokens: None, memory_pool_id: None,
        skill_ids: &[s1.id.clone()],
        capabilities: &[],
    }).await.unwrap();

    let new_ids = vec![s2.id.clone()];
    cat.agents().update(&t.id, &agent.id, AgentPatch {
        skill_ids: Some(&new_ids),
        ..Default::default()
    }).await.unwrap();

    let resolved = cat.agents().resolve(&t.id, "x").await.unwrap();
    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.skills[0].name, "b");
}

#[tokio::test]
async fn agent_update_with_empty_skill_ids_clears_set() {
    let cat = fresh().await;
    let t = cat.tenants().create("t", None).await.unwrap();
    let s = cat.skills().create(&t.id, NewSkill { name: "a", description: None, body: "a", metadata: None }).await.unwrap();
    let agent = cat.agents().create(&t.id, NewAgent {
        name: "x", description: None, system_prompt: "p", model: "m",
        max_turns: None, max_tokens: None, memory_pool_id: None,
        skill_ids: &[s.id.clone()], capabilities: &[],
    }).await.unwrap();

    let empty: Vec<simulacra_catalog::SkillId> = vec![];
    cat.agents().update(&t.id, &agent.id, AgentPatch {
        skill_ids: Some(&empty),
        ..Default::default()
    }).await.unwrap();

    let resolved = cat.agents().resolve(&t.id, "x").await.unwrap();
    assert!(resolved.skills.is_empty());
}

#[tokio::test]
async fn agent_delete_cascades_skills_and_capabilities() {
    let cat = fresh().await;
    let t = cat.tenants().create("t", None).await.unwrap();
    let s = cat.skills().create(&t.id, NewSkill { name: "a", description: None, body: "a", metadata: None }).await.unwrap();

    let agent = cat.agents().create(&t.id, NewAgent {
        name: "x", description: None, system_prompt: "p", model: "m",
        max_turns: None, max_tokens: None, memory_pool_id: None,
        skill_ids: &[s.id.clone()],
        capabilities: &["net:*".to_string()],
    }).await.unwrap();

    cat.agents().delete(&t.id, &agent.id).await.unwrap();

    // Verify skills row still exists (skill outlives agent)
    let s_again = cat.skills().get(&t.id, &s.id).await.unwrap();
    assert_eq!(s_again.id.as_str(), s.id.as_str());

    // Verify agent_skills + agent_capabilities cleaned up via direct SQL
    let conn = cat.conn_for_tests();
    let conn = conn.lock().unwrap();
    let asn: i64 = conn.query_row("SELECT COUNT(*) FROM agent_skills WHERE agent_id = ?1", [agent.id.as_str()], |r| r.get(0)).unwrap();
    assert_eq!(asn, 0);
    let acn: i64 = conn.query_row("SELECT COUNT(*) FROM agent_capabilities WHERE agent_id = ?1", [agent.id.as_str()], |r| r.get(0)).unwrap();
    assert_eq!(acn, 0);
}

#[tokio::test]
async fn agent_cross_tenant_get_returns_not_found() {
    let cat = fresh().await;
    let alice = cat.tenants().create("alice", None).await.unwrap();
    let bob   = cat.tenants().create("bob", None).await.unwrap();

    let alice_agent = cat.agents().create(&alice.id, NewAgent {
        name: "x", description: None, system_prompt: "p", model: "m",
        max_turns: None, max_tokens: None, memory_pool_id: None,
        skill_ids: &[], capabilities: &[],
    }).await.unwrap();

    let err = cat.agents().get(&bob.id, &alice_agent.id).await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));

    let err = cat.agents().resolve(&bob.id, "x").await.unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn skill_list_paginates_with_stable_cursor() {
    let cat = fresh().await;
    let t = cat.tenants().create("t", None).await.unwrap();
    let skills = cat.skills();

    for i in 0..5u8 {
        skills.create(&t.id, NewSkill {
            name: &format!("s{i}"),
            description: None, body: "x", metadata: None,
        }).await.unwrap();
    }

    let p1 = skills.list(&t.id, simulacra_catalog::PageRequest {
        first: Some(2), after: None, last: None, before: None,
    }).await.unwrap();
    assert_eq!(p1.items.len(), 2);
    assert!(p1.has_next_page);

    let p2 = skills.list(&t.id, simulacra_catalog::PageRequest {
        first: Some(2), after: p1.end_cursor.clone(), last: None, before: None,
    }).await.unwrap();
    assert_eq!(p2.items.len(), 2);
    assert_ne!(p1.items[0].id.as_str(), p2.items[0].id.as_str());
}
```

- [ ] **Step 4: Run tests; iterate impl until all pass**

```bash
cargo test -p simulacra-catalog --tests
```

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-catalog/
git commit -m "feat(simulacra-catalog): sqlite agent/skill repos with resolve [S042]"
```

---

### Task 4: In-memory repository implementations (read-only fakes for `--no-catalog`)

**Files:**
- Create: `crates/simulacra-catalog/src/repo/memory/mod.rs`
- Create: `crates/simulacra-catalog/src/repo/memory/tenant.rs`
- Create: `crates/simulacra-catalog/src/repo/memory/agent.rs`
- Create: `crates/simulacra-catalog/src/repo/memory/skill.rs`
- Create: `crates/simulacra-catalog/src/repo/memory/memory_pool.rs`
- Create: `crates/simulacra-catalog/tests/memory_repos.rs`

- [ ] **Step 1: Create `src/repo/memory/mod.rs`**

```rust
use std::collections::HashMap;
use std::sync::Arc;

use crate::ids::{AgentId, MemoryPoolId, SkillId, TenantId};
use crate::models::{Agent, MemoryPool, Skill, Tenant};

pub mod tenant;
pub mod agent;
pub mod skill;
pub mod memory_pool;

pub use tenant::MemoryTenantRepository;
pub use agent::MemoryAgentRepository;
pub use skill::MemorySkillRepository;
pub use memory_pool::MemoryMemoryPoolRepository;

/// Container for fixtures populated at CLI bootstrap when `--no-catalog`
/// is in effect. All maps are keyed by id; lookups by name iterate.
#[derive(Default)]
pub struct InMemoryFixtures {
    pub tenants: HashMap<TenantId, Tenant>,
    pub agents: HashMap<AgentId, Agent>,
    pub agent_skills: HashMap<AgentId, Vec<SkillId>>,
    pub agent_capabilities: HashMap<AgentId, Vec<String>>,
    pub skills: HashMap<SkillId, Skill>,
    pub memory_pools: HashMap<MemoryPoolId, MemoryPool>,
}

pub type SharedFixtures = Arc<InMemoryFixtures>;
```

- [ ] **Step 2: Create individual repo files**

Each holds an `Arc<InMemoryFixtures>`. Read methods iterate/lookup; mutating methods return `CatalogError::ReadOnly("--no-catalog mode")`. `list` paginates over a sorted `Vec`. `resolve` walks the in-memory joins.

```rust
// agent.rs sketch
pub struct MemoryAgentRepository { fx: SharedFixtures }

impl MemoryAgentRepository {
    pub fn new(fx: SharedFixtures) -> Self { Self { fx } }
}

#[async_trait]
impl AgentRepository for MemoryAgentRepository {
    async fn get_by_name(&self, tenant_id: &TenantId, name: &str) -> Result<Agent, CatalogError> {
        self.fx.agents.values()
            .find(|a| &a.tenant_id == tenant_id && a.name == name)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound(format!("agent name={name}")))
    }

    async fn resolve(&self, tenant_id: &TenantId, name: &str) -> Result<ResolvedAgent, CatalogError> {
        let agent = self.get_by_name(tenant_id, name).await?;
        let skill_ids = self.fx.agent_skills.get(&agent.id).cloned().unwrap_or_default();
        let skills: Vec<Skill> = skill_ids.iter()
            .filter_map(|id| self.fx.skills.get(id).cloned())
            .collect();
        let capabilities = self.fx.agent_capabilities.get(&agent.id).cloned().unwrap_or_default();
        let memory_pool = agent.memory_pool_id.as_ref()
            .and_then(|id| self.fx.memory_pools.get(id).cloned());
        Ok(ResolvedAgent {
            id: agent.id, name: agent.name, system_prompt: agent.system_prompt,
            model: agent.model, max_turns: agent.max_turns, max_tokens: agent.max_tokens,
            skills, capabilities, memory_pool,
        })
    }

    async fn create(&self, _: &TenantId, _: NewAgent<'_>) -> Result<Agent, CatalogError> {
        Err(CatalogError::ReadOnly("--no-catalog mode does not support agent creation".into()))
    }
    // update, delete: same ReadOnly error
    // get, list, capabilities: read-only happy paths
}
```

Same shape for tenant/skill/memory_pool repos.

- [ ] **Step 3: Write `tests/memory_repos.rs`**

```rust
use std::sync::Arc;
use simulacra_catalog::repo::{AgentRepository, memory::{MemoryAgentRepository, InMemoryFixtures}};
use simulacra_catalog::ids::{AgentId, TenantId};
use simulacra_catalog::models::{Agent, NewAgent};
use chrono::Utc;

fn build_fx() -> Arc<InMemoryFixtures> {
    let mut fx = InMemoryFixtures::default();
    let t_id = TenantId("default".into());
    fx.tenants.insert(t_id.clone(), simulacra_catalog::models::Tenant {
        id: t_id.clone(), namespace: "default".into(), display_name: None,
        created_at: Utc::now(), updated_at: Utc::now(),
    });
    let a_id = AgentId("a1".into());
    fx.agents.insert(a_id.clone(), Agent {
        id: a_id.clone(), tenant_id: t_id.clone(), name: "assistant".into(),
        description: None, system_prompt: "p".into(), model: "m".into(),
        max_turns: 100, max_tokens: None, memory_pool_id: None,
        created_at: Utc::now(), updated_at: Utc::now(),
    });
    fx.agent_capabilities.insert(a_id.clone(), vec!["net:*".into()]);
    Arc::new(fx)
}

#[tokio::test]
async fn resolve_serves_in_memory_agent() {
    let fx = build_fx();
    let repo = MemoryAgentRepository::new(fx.clone());
    let resolved = repo.resolve(&TenantId("default".into()), "assistant").await.unwrap();
    assert_eq!(resolved.name, "assistant");
    assert_eq!(resolved.capabilities, vec!["net:*"]);
}

#[tokio::test]
async fn create_returns_readonly_error() {
    let fx = build_fx();
    let repo = MemoryAgentRepository::new(fx);
    let err = repo.create(&TenantId("default".into()), NewAgent {
        name: "x", description: None, system_prompt: "p", model: "m",
        max_turns: None, max_tokens: None, memory_pool_id: None,
        skill_ids: &[], capabilities: &[],
    }).await.unwrap_err();
    assert!(matches!(err, simulacra_catalog::CatalogError::ReadOnly(_)));
}
```

- [ ] **Step 4: Run tests; iterate**

```bash
cargo test -p simulacra-catalog --tests
```

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-catalog/
git commit -m "feat(simulacra-catalog): in-memory repos for --no-catalog [S042]"
```

---

### Task 5: `CatalogSkillFs` — VFS layer that exposes catalog skills

**Files:**
- Create: `crates/simulacra-catalog/src/skill_fs.rs`
- Create: `crates/simulacra-catalog/tests/skill_fs.rs`

- [ ] **Step 1: Inspect the `FsLayer` trait you must implement**

Run: `rg "trait FsLayer" crates/simulacra-vfs/src -A 80 | head -120`. Note the methods (typically `list_dir`, `read`, `stat`, `write`, `remove`) and how `Errno` maps. Mirror `WasmVfsLayer` (S040) for the pattern.

- [ ] **Step 2: Create `src/skill_fs.rs`**

```rust
use std::sync::Arc;

use simulacra_vfs::{Errno, FsLayer, /* DirEntry, FileStat, OpenFlags — match the actual trait */};
use crate::models::Skill;

/// Read-only FsLayer that exposes a snapshot of skills as `<name>.md` files
/// at the mount root. Composed alongside any host-mounted skills layer.
pub struct CatalogSkillFs {
    skills: Arc<Vec<Skill>>,
}

impl CatalogSkillFs {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills: Arc::new(skills) }
    }

    fn render(skill: &Skill) -> String {
        let mut out = String::new();
        if let Some(meta) = &skill.metadata {
            // Render as YAML frontmatter so S017 skill discovery parses it
            out.push_str("---\n");
            // Use a simple `serde_yaml` round-trip if present; otherwise emit
            // a stable subset by hand. Keep this in sync with how the existing
            // host-mounted skill files look on disk.
            if let Ok(yaml) = serde_yaml::to_string(meta) {
                out.push_str(&yaml);
            }
            out.push_str("---\n\n");
        }
        out.push_str(&skill.body);
        out
    }
}

impl FsLayer for CatalogSkillFs {
    // Replace each method body to match the actual trait:
    fn list_dir(&self, path: &str) -> Result<Vec<String>, Errno> {
        if path != "/" && path != "" { return Err(Errno::NOENT); }
        Ok(self.skills.iter().map(|s| format!("{}.md", s.name)).collect())
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, Errno> {
        let name = path.trim_start_matches('/').strip_suffix(".md").ok_or(Errno::NOENT)?;
        let skill = self.skills.iter().find(|s| s.name == name).ok_or(Errno::NOENT)?;
        Ok(Self::render(skill).into_bytes())
    }

    fn stat(&self, path: &str) -> Result<simulacra_vfs::FileStat, Errno> {
        // Stub the exact return type; mirror WasmVfsLayer
        unimplemented!("match FsLayer trait shape")
    }

    fn write(&self, _path: &str, _data: &[u8]) -> Result<(), Errno> { Err(Errno::ROFS) }
    fn remove(&self, _path: &str) -> Result<(), Errno> { Err(Errno::ROFS) }
}
```

Add `serde_yaml = "0.9"` to `simulacra-catalog` dependencies if frontmatter rendering needs YAML serialization; otherwise emit by hand.

- [ ] **Step 3: Write `tests/skill_fs.rs`**

```rust
use simulacra_catalog::CatalogSkillFs;
use simulacra_catalog::models::Skill;
use simulacra_catalog::ids::{SkillId, TenantId};
use simulacra_vfs::{Errno, FsLayer};
use serde_json::json;
use chrono::Utc;

fn skill(name: &str, body: &str, meta: Option<serde_json::Value>) -> Skill {
    Skill {
        id: SkillId::new(),
        tenant_id: TenantId("t".into()),
        name: name.into(),
        description: None,
        body: body.into(),
        metadata: meta,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

#[test]
fn list_dir_root_returns_skill_filenames() {
    let fs = CatalogSkillFs::new(vec![skill("alpha", "a", None), skill("beta", "b", None)]);
    let entries = fs.list_dir("/").unwrap();
    assert!(entries.contains(&"alpha.md".to_string()));
    assert!(entries.contains(&"beta.md".to_string()));
}

#[test]
fn read_renders_frontmatter_and_body() {
    let fs = CatalogSkillFs::new(vec![skill(
        "x", "Hello body.",
        Some(json!({"name": "x", "description": "d"})),
    )]);
    let bytes = fs.read("/x.md").unwrap();
    let s = String::from_utf8(bytes).unwrap();
    assert!(s.starts_with("---\n"), "expected frontmatter delimiter, got: {s}");
    assert!(s.contains("description"), "missing metadata: {s}");
    assert!(s.contains("Hello body."));
}

#[test]
fn read_missing_returns_noent() {
    let fs = CatalogSkillFs::new(vec![]);
    assert!(matches!(fs.read("/nope.md"), Err(Errno::NOENT)));
}

#[test]
fn write_returns_rofs() {
    let fs = CatalogSkillFs::new(vec![skill("x", "x", None)]);
    assert!(matches!(fs.write("/x.md", b"new"), Err(Errno::ROFS)));
}

#[test]
fn remove_returns_rofs() {
    let fs = CatalogSkillFs::new(vec![skill("x", "x", None)]);
    assert!(matches!(fs.remove("/x.md"), Err(Errno::ROFS)));
}
```

- [ ] **Step 4: Run tests; iterate**

```bash
cargo test -p simulacra-catalog --test skill_fs
```

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-catalog/
git commit -m "feat(simulacra-catalog): CatalogSkillFs VFS layer [S042]"
```

---

### Task 6: Scaffold `simulacra-graphql` crate with schema types and context

**Files:**
- Create: `crates/simulacra-graphql/Cargo.toml`
- Create: `crates/simulacra-graphql/src/lib.rs`
- Create: `crates/simulacra-graphql/src/error.rs`
- Create: `crates/simulacra-graphql/src/context.rs`
- Create: `crates/simulacra-graphql/src/schema/mod.rs`
- Create: `crates/simulacra-graphql/src/schema/scalars.rs`
- Create: `crates/simulacra-graphql/src/schema/connection.rs`
- Create: `crates/simulacra-graphql/src/schema/agent.rs` (types only — resolvers come next)
- Create: `crates/simulacra-graphql/src/schema/skill.rs` (types only)
- Create: `crates/simulacra-graphql/src/schema/memory_pool.rs` (types only)

- [ ] **Step 1: Create `crates/simulacra-graphql/Cargo.toml`**

```toml
[package]
name = "simulacra-graphql"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
simulacra-catalog.workspace = true
async-graphql.workspace = true
async-graphql-axum.workspace = true
axum.workspace = true
chrono = { workspace = true, features = ["serde"] }
opentelemetry.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
tempfile = "3"
tower = "0.5"
http = "1"
hyper = { workspace = true, features = ["full"] }
```

**Avoiding a circular dependency.** `simulacra-graphql` does NOT depend on `simulacra-server` (which would later depend on `simulacra-graphql` for the route mount, creating a cycle). Instead, `simulacra-graphql` defines its own minimal auth trait and principal type. `simulacra-server` will adapt S031's existing `AuthProvider` to this trait at the point where it constructs the router (Task 10).

```rust
// crates/simulacra-graphql/src/auth.rs (definition; impl in Task 9)
#[async_trait::async_trait]
pub trait GraphQLAuthProvider: Send + Sync {
    async fn authenticate(&self, headers: &http::HeaderMap) -> Result<AuthPrincipal, AuthError>;
}

#[derive(Clone)]
pub struct AuthPrincipal {
    pub tenant_namespace: String,
    pub subject: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("forbidden")]
    Forbidden,
}
```

- [ ] **Step 2: Create `src/error.rs`**

```rust
use async_graphql::{ErrorExtensions, FieldError};
use simulacra_catalog::CatalogError;

pub fn to_field_error(err: CatalogError) -> FieldError {
    let (code, message) = match &err {
        CatalogError::NotFound(m) => ("NOT_FOUND", m.clone()),
        CatalogError::Conflict(m) => ("CONFLICT", m.clone()),
        CatalogError::Validation(m) => ("VALIDATION", m.clone()),
        CatalogError::ReadOnly(m) => ("READ_ONLY", m.clone()),
        _ => ("INTERNAL", err.to_string()),
    };
    FieldError::new(message).extend_with(|_, e| e.set("code", code))
}
```

- [ ] **Step 3: Create `src/schema/scalars.rs`**

```rust
use async_graphql::{Scalar, ScalarType, Value, InputValueError, InputValueResult};
use chrono::{DateTime, Utc};

pub struct DateTimeScalar(pub DateTime<Utc>);

#[Scalar(name = "DateTime")]
impl ScalarType for DateTimeScalar {
    fn parse(value: Value) -> InputValueResult<Self> {
        match value {
            Value::String(s) => DateTime::parse_from_rfc3339(&s)
                .map(|dt| DateTimeScalar(dt.with_timezone(&Utc)))
                .map_err(|e| InputValueError::custom(e.to_string())),
            _ => Err(InputValueError::expected_type(value)),
        }
    }
    fn to_value(&self) -> Value {
        Value::String(self.0.to_rfc3339())
    }
}

// JSON scalar — async-graphql ships one; re-export or alias here.
```

- [ ] **Step 4: Create `src/context.rs` with tenant cache**

```rust
use std::collections::HashMap;
use std::sync::Arc;

use simulacra_catalog::ids::TenantId;
use simulacra_catalog::repo::TenantRepository;
use parking_lot::RwLock;

#[derive(Clone)]
pub struct AuthenticatedPrincipal {
    pub tenant_namespace: String,
    pub subject: String,
}

#[derive(Clone)]
pub struct GraphQLContext {
    pub tenant_id: TenantId,
    pub principal: AuthenticatedPrincipal,
}

#[derive(Clone)]
pub struct TenantResolver {
    repo: Arc<dyn TenantRepository>,
    cache: Arc<RwLock<HashMap<String, TenantId>>>,
}

impl TenantResolver {
    pub fn new(repo: Arc<dyn TenantRepository>) -> Self {
        Self { repo, cache: Arc::new(RwLock::new(HashMap::new())) }
    }

    pub async fn resolve(&self, namespace: &str) -> Result<TenantId, simulacra_catalog::CatalogError> {
        if let Some(id) = self.cache.read().get(namespace) {
            return Ok(id.clone());
        }
        let tenant = self.repo.get_by_namespace(namespace).await?;
        self.cache.write().insert(namespace.to_owned(), tenant.id.clone());
        Ok(tenant.id)
    }

    pub fn invalidate(&self, namespace: &str) {
        self.cache.write().remove(namespace);
    }
}
```

(Add `parking_lot = "0.12"` to deps.)

- [ ] **Step 5: Create GraphQL types (Agent, Skill, MemoryPool) in their respective `schema/*.rs` files — types only**

```rust
// schema/agent.rs (types)
use async_graphql::*;
use chrono::{DateTime, Utc};
use simulacra_catalog::models::Agent as AgentModel;

pub struct AgentNode(pub AgentModel);

#[Object(name = "Agent")]
impl AgentNode {
    async fn id(&self) -> ID { ID(self.0.id.0.clone()) }
    async fn name(&self) -> &str { &self.0.name }
    async fn description(&self) -> Option<&str> { self.0.description.as_deref() }
    async fn system_prompt(&self) -> &str { &self.0.system_prompt }
    async fn model(&self) -> &str { &self.0.model }
    async fn max_turns(&self) -> i32 { self.0.max_turns as i32 }
    async fn max_tokens(&self) -> Option<i32> { self.0.max_tokens.map(|n| n as i32) }
    async fn created_at(&self) -> DateTime<Utc> { self.0.created_at }
    async fn updated_at(&self) -> DateTime<Utc> { self.0.updated_at }

    // Skills, capabilities, memoryPool resolvers added in Task 7 alongside queries
}
```

Mirror for `SkillNode` and `MemoryPoolNode`.

- [ ] **Step 6: Create `src/schema/connection.rs` with cursor helpers**

```rust
use async_graphql::*;
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD as B64};

#[derive(SimpleObject)]
pub struct PageInfoExt {
    pub has_next_page: bool,
    pub has_previous_page: bool,
    pub start_cursor: Option<String>,
    pub end_cursor: Option<String>,
}

pub fn encode_cursor(created_at: chrono::DateTime<chrono::Utc>, id: &str) -> String {
    B64.encode(format!("{}|{}", created_at.to_rfc3339(), id))
}
```

- [ ] **Step 7: Create `src/schema/mod.rs` and `src/lib.rs` skeletons**

```rust
// schema/mod.rs
pub mod agent;
pub mod skill;
pub mod memory_pool;
pub mod scalars;
pub mod connection;

use async_graphql::{EmptySubscription, Schema};

pub type SimulacraSchema = Schema<crate::schema::QueryRoot, crate::schema::MutationRoot, EmptySubscription>;

pub struct QueryRoot;
pub struct MutationRoot;

// Concrete query/mutation implementations live in agent.rs etc. and are
// merged here via async-graphql's `MergedObject` derive in Task 7/8.
```

```rust
// lib.rs
pub mod context;
pub mod error;
pub mod schema;

// Public surface for simulacra-server to mount the route is added in Task 9.
```

- [ ] **Step 8: Run `cargo build -p simulacra-graphql`; iterate until clean**

```bash
cargo build -p simulacra-graphql
```

- [ ] **Step 9: Commit**

```bash
git add crates/simulacra-graphql/ Cargo.toml
git commit -m "feat(simulacra-graphql): scaffold crate with types, context, scalars [S042]"
```

---

### Task 7: GraphQL queries with pagination + filters

**Files:**
- Modify: `crates/simulacra-graphql/src/schema/agent.rs`
- Modify: `crates/simulacra-graphql/src/schema/skill.rs`
- Modify: `crates/simulacra-graphql/src/schema/memory_pool.rs`
- Modify: `crates/simulacra-graphql/src/schema/mod.rs` (merge query roots)
- Create: `crates/simulacra-graphql/tests/queries.rs`

- [ ] **Step 1: Add resolver fields to `AgentNode` for joined data**

```rust
#[Object(name = "Agent")]
impl AgentNode {
    // ... id/name/etc from Task 6 ...

    async fn skills(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<crate::schema::skill::SkillNode>> {
        let skills_repo = ctx.data_unchecked::<Arc<dyn SkillRepository>>();
        let gql_ctx = ctx.data_unchecked::<GraphQLContext>();
        let skills = skills_repo.list_for_agent(&gql_ctx.tenant_id, &self.0.id)
            .await
            .map_err(crate::error::to_field_error)?;
        Ok(skills.into_iter().map(crate::schema::skill::SkillNode).collect())
    }

    async fn capabilities(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<String>> {
        let agents = ctx.data_unchecked::<Arc<dyn AgentRepository>>();
        agents.capabilities(&self.0.id).await.map_err(crate::error::to_field_error)
    }

    async fn memory_pool(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<crate::schema::memory_pool::MemoryPoolNode>> {
        match &self.0.memory_pool_id {
            None => Ok(None),
            Some(id) => {
                let pools = ctx.data_unchecked::<Arc<dyn MemoryPoolRepository>>();
                let gql_ctx = ctx.data_unchecked::<GraphQLContext>();
                let pool = pools.get(&gql_ctx.tenant_id, id).await
                    .map_err(crate::error::to_field_error)?;
                Ok(Some(crate::schema::memory_pool::MemoryPoolNode(pool)))
            }
        }
    }
}
```

- [ ] **Step 2: Define `AgentQuery`, `SkillQuery`, `MemoryPoolQuery` and merge**

```rust
// schema/agent.rs
#[derive(Default)]
pub struct AgentQuery;

#[Object]
impl AgentQuery {
    async fn agent(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<Option<AgentNode>> {
        let repo = ctx.data_unchecked::<Arc<dyn AgentRepository>>();
        let gql_ctx = ctx.data_unchecked::<GraphQLContext>();
        match repo.get(&gql_ctx.tenant_id, &AgentId(id.0.clone())).await {
            Ok(a) => Ok(Some(AgentNode(a))),
            Err(simulacra_catalog::CatalogError::NotFound(_)) => Ok(None),
            Err(e) => Err(crate::error::to_field_error(e)),
        }
    }

    async fn agents(
        &self, ctx: &Context<'_>,
        filter: Option<AgentFilter>, page: Option<PageInput>,
    ) -> async_graphql::Result<AgentConnection> {
        let repo = ctx.data_unchecked::<Arc<dyn AgentRepository>>();
        let gql_ctx = ctx.data_unchecked::<GraphQLContext>();
        // Apply filter post-query for v1 (small data sets); push to SQL later
        let req = page.map(Into::into).unwrap_or_default();
        let raw = repo.list(&gql_ctx.tenant_id, req).await
            .map_err(crate::error::to_field_error)?;
        Ok(to_connection(raw, filter))
    }
}

// schema/mod.rs
#[derive(MergedObject, Default)]
pub struct QueryRoot(
    crate::schema::agent::AgentQuery,
    crate::schema::skill::SkillQuery,
    crate::schema::memory_pool::MemoryPoolQuery,
);
```

- [ ] **Step 3: Define `AgentConnection`, `AgentEdge`, `PageInput`, `AgentFilter`**

```rust
#[derive(InputObject, Default)]
pub struct PageInput {
    pub first: Option<i32>,
    pub after: Option<String>,
    pub last: Option<i32>,
    pub before: Option<String>,
}

impl From<PageInput> for simulacra_catalog::PageRequest {
    fn from(p: PageInput) -> Self {
        Self {
            first: p.first.map(|n| n.max(0) as u32),
            after: p.after,
            last: p.last.map(|n| n.max(0) as u32),
            before: p.before,
        }
    }
}

#[derive(InputObject)]
pub struct AgentFilter {
    pub name_contains: Option<String>,
}

#[derive(SimpleObject)]
pub struct AgentEdge {
    pub node: AgentNode,
    pub cursor: String,
}

#[derive(SimpleObject)]
pub struct AgentConnection {
    pub edges: Vec<AgentEdge>,
    pub page_info: crate::schema::connection::PageInfoExt,
}
```

- [ ] **Step 4: Helper `to_connection`**

```rust
fn to_connection(p: simulacra_catalog::Page<simulacra_catalog::Agent>, filter: Option<AgentFilter>) -> AgentConnection {
    let edges = p.items.into_iter()
        .filter(|a| match &filter {
            Some(f) => f.name_contains.as_ref()
                .map_or(true, |needle| a.name.contains(needle)),
            None => true,
        })
        .map(|a| {
            let cursor = crate::schema::connection::encode_cursor(a.created_at, a.id.as_str());
            AgentEdge { node: AgentNode(a), cursor }
        })
        .collect::<Vec<_>>();

    let start_cursor = edges.first().map(|e| e.cursor.clone());
    let end_cursor = edges.last().map(|e| e.cursor.clone());

    AgentConnection {
        edges,
        page_info: crate::schema::connection::PageInfoExt {
            has_next_page: p.has_next_page,
            has_previous_page: p.has_previous_page,
            start_cursor,
            end_cursor,
        },
    }
}
```

Mirror for skills and memory_pools.

- [ ] **Step 5: Write `tests/queries.rs`**

```rust
use std::sync::Arc;
use async_graphql::{Schema, EmptyMutation, EmptySubscription, Value};
use simulacra_catalog::{Catalog, ids::TenantId};
use simulacra_catalog::repo::{AgentRepository, MemoryPoolRepository, SkillRepository};
use simulacra_graphql::context::{GraphQLContext, AuthenticatedPrincipal};
use simulacra_graphql::schema::QueryRoot;

async fn schema_with_seeded_catalog() -> (Schema<QueryRoot, EmptyMutation, EmptySubscription>, TenantId) {
    let cat = Catalog::open_in_memory().unwrap();
    let tenant = cat.tenants().create("acme", None).await.unwrap();

    let s = cat.skills().create(&tenant.id, simulacra_catalog::models::NewSkill {
        name: "summarize", description: None, body: "# do it", metadata: None,
    }).await.unwrap();

    cat.agents().create(&tenant.id, simulacra_catalog::models::NewAgent {
        name: "assistant", description: None, system_prompt: "p", model: "m",
        max_turns: None, max_tokens: None, memory_pool_id: None,
        skill_ids: &[s.id.clone()], capabilities: &["net:*".into()],
    }).await.unwrap();

    let agent_repo: Arc<dyn AgentRepository> = Arc::new(cat.agents());
    let skill_repo: Arc<dyn SkillRepository> = Arc::new(cat.skills());
    let pool_repo:  Arc<dyn MemoryPoolRepository> = Arc::new(cat.memory_pools());

    let schema = Schema::build(QueryRoot::default(), EmptyMutation, EmptySubscription)
        .data(agent_repo).data(skill_repo).data(pool_repo)
        .data(GraphQLContext {
            tenant_id: tenant.id.clone(),
            principal: AuthenticatedPrincipal { tenant_namespace: "acme".into(), subject: "test".into() },
        })
        .finish();

    (schema, tenant.id)
}

#[tokio::test]
async fn agent_query_returns_node_with_skills_and_capabilities() {
    let (schema, _) = schema_with_seeded_catalog().await;
    let resp = schema.execute(r#"{
        agents { edges { node { name skills { name } capabilities } } }
    }"#).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);

    let data = resp.data.into_json().unwrap();
    let edges = &data["agents"]["edges"];
    assert_eq!(edges[0]["node"]["name"], "assistant");
    assert_eq!(edges[0]["node"]["skills"][0]["name"], "summarize");
    assert_eq!(edges[0]["node"]["capabilities"][0], "net:*");
}

#[tokio::test]
async fn agents_pagination_round_trips_cursor() {
    // create 5 agents, query first 2, then after end_cursor for next 2
    // assert ids don't overlap
    // (full body omitted for brevity — pattern mirrors sqlite_repos pagination test)
}
```

- [ ] **Step 6: Run tests; iterate**

```bash
cargo test -p simulacra-graphql --test queries
```

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-graphql/
git commit -m "feat(simulacra-graphql): query resolvers with relay pagination [S042]"
```

---

### Task 8: GraphQL mutations (create/update/delete for Agent/Skill/MemoryPool)

**Files:**
- Modify: `crates/simulacra-graphql/src/schema/agent.rs`
- Modify: `crates/simulacra-graphql/src/schema/skill.rs`
- Modify: `crates/simulacra-graphql/src/schema/memory_pool.rs`
- Modify: `crates/simulacra-graphql/src/schema/mod.rs` (merge mutation roots)
- Create: `crates/simulacra-graphql/tests/mutations.rs`

- [ ] **Step 1: Define inputs and `AgentMutation`**

```rust
#[derive(InputObject)]
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
}

#[derive(InputObject, Default)]
pub struct UpdateAgentInput {
    pub description: Option<Option<String>>,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub max_turns: Option<i32>,
    pub max_tokens: Option<Option<i32>>,
    pub skill_ids: Option<Vec<ID>>,
    pub capabilities: Option<Vec<String>>,
    pub memory_pool_id: Option<Option<ID>>,
}

#[derive(Default)]
pub struct AgentMutation;

#[Object]
impl AgentMutation {
    async fn create_agent(&self, ctx: &Context<'_>, input: CreateAgentInput) -> async_graphql::Result<AgentNode> {
        let repo = ctx.data_unchecked::<Arc<dyn AgentRepository>>();
        let gql_ctx = ctx.data_unchecked::<GraphQLContext>();

        let skill_ids: Vec<SkillId> = input.skill_ids.into_iter().map(|i| SkillId(i.0)).collect();
        let caps = input.capabilities.clone();
        let pool = input.memory_pool_id.as_ref().map(|i| MemoryPoolId(i.0.clone()));

        let agent = repo.create(&gql_ctx.tenant_id, NewAgent {
            name: &input.name,
            description: input.description.as_deref(),
            system_prompt: &input.system_prompt,
            model: &input.model,
            max_turns: input.max_turns.map(|n| n.max(0) as u32),
            max_tokens: input.max_tokens.map(|n| n.max(0) as u32),
            memory_pool_id: pool.as_ref(),
            skill_ids: &skill_ids,
            capabilities: &caps,
        })
        .await
        .map_err(crate::error::to_field_error)?;

        Ok(AgentNode(agent))
    }

    async fn update_agent(&self, ctx: &Context<'_>, id: ID, input: UpdateAgentInput)
        -> async_graphql::Result<AgentNode>
    {
        let repo = ctx.data_unchecked::<Arc<dyn AgentRepository>>();
        let gql_ctx = ctx.data_unchecked::<GraphQLContext>();

        let skill_ids = input.skill_ids.as_ref().map(|v| v.iter().map(|i| SkillId(i.0.clone())).collect::<Vec<_>>());
        let caps = input.capabilities.as_ref().cloned();
        let pool_owned: Option<Option<MemoryPoolId>> = input.memory_pool_id
            .map(|opt| opt.map(|i| MemoryPoolId(i.0)));

        let patch = AgentPatch {
            description: input.description.map(|opt| opt.as_deref()),
            system_prompt: input.system_prompt.as_deref(),
            model: input.model.as_deref(),
            max_turns: input.max_turns.map(|n| n.max(0) as u32),
            max_tokens: input.max_tokens.map(|opt| opt.map(|n| n.max(0) as u32)),
            memory_pool_id: pool_owned.as_ref().map(|opt| opt.as_ref()),
            skill_ids: skill_ids.as_deref(),
            capabilities: caps.as_deref(),
        };

        let agent = repo.update(&gql_ctx.tenant_id, &AgentId(id.0), patch).await
            .map_err(crate::error::to_field_error)?;
        Ok(AgentNode(agent))
    }

    async fn delete_agent(&self, ctx: &Context<'_>, id: ID) -> async_graphql::Result<bool> {
        let repo = ctx.data_unchecked::<Arc<dyn AgentRepository>>();
        let gql_ctx = ctx.data_unchecked::<GraphQLContext>();
        repo.delete(&gql_ctx.tenant_id, &AgentId(id.0)).await
            .map_err(crate::error::to_field_error)?;
        Ok(true)
    }
}
```

Note: lifetime juggling between async-graphql input lifetimes and the `&str` shape of `NewAgent<'_>` may need owned variants of the new/patch structs for ergonomic GraphQL bridging. If the borrow checker pushes back, add `OwnedNewAgent` etc. in `simulacra-catalog::models` and have repos accept `Into<NewAgent>`.

- [ ] **Step 2: Define `SkillMutation` and `MemoryPoolMutation` mirrors**

Mirror the agent mutation pattern.

- [ ] **Step 3: Merge mutations in `schema/mod.rs`**

```rust
#[derive(MergedObject, Default)]
pub struct MutationRoot(
    crate::schema::agent::AgentMutation,
    crate::schema::skill::SkillMutation,
    crate::schema::memory_pool::MemoryPoolMutation,
);
```

- [ ] **Step 4: Write `tests/mutations.rs`**

```rust
#[tokio::test]
async fn create_agent_returns_full_node() {
    let (schema, _tenant_id) = schema_with_seeded_catalog().await;
    let mutation = r#"
        mutation {
            createAgent(input: {
                name: "newbie",
                systemPrompt: "p",
                model: "m",
                skillIds: [],
                capabilities: []
            }) { id name }
        }
    "#;
    let resp = schema.execute(mutation).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["createAgent"]["name"], "newbie");
}

#[tokio::test]
async fn create_agent_with_duplicate_name_returns_conflict_code() {
    let (schema, _) = schema_with_seeded_catalog().await;
    // first create
    let _ = schema.execute(r#"mutation { createAgent(input: { name: "dup", systemPrompt: "p", model: "m", skillIds: [], capabilities: [] }) { id } }"#).await;
    // second create — same name
    let resp = schema.execute(r#"mutation { createAgent(input: { name: "dup", systemPrompt: "p", model: "m", skillIds: [], capabilities: [] }) { id } }"#).await;
    assert!(!resp.errors.is_empty());
    let ext = &resp.errors[0].extensions;
    assert_eq!(ext.as_ref().unwrap().get("code").and_then(|v| v.clone().into_json().ok()).unwrap(), serde_json::json!("CONFLICT"));
}

#[tokio::test]
async fn create_agent_with_unknown_skill_id_returns_validation_error() {
    let (schema, _) = schema_with_seeded_catalog().await;
    let resp = schema.execute(r#"mutation {
        createAgent(input: { name: "x", systemPrompt: "p", model: "m", skillIds: ["does-not-exist"], capabilities: [] }) { id }
    }"#).await;
    assert!(!resp.errors.is_empty());
    // assert code == VALIDATION
}

#[tokio::test]
async fn update_agent_with_null_skill_ids_preserves_set() {
    // create agent with [s1]; update with skillIds: null; assert skills still [s1]
}

#[tokio::test]
async fn update_agent_with_empty_skill_ids_clears_set() {
    // create agent with [s1]; update with skillIds: []; assert skills empty
}

#[tokio::test]
async fn delete_agent_returns_true_and_makes_it_unqueryable() {
    let (schema, _) = schema_with_seeded_catalog().await;
    // create then delete then query — expect null
}
```

- [ ] **Step 5: Run tests; iterate**

```bash
cargo test -p simulacra-graphql --test mutations
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-graphql/
git commit -m "feat(simulacra-graphql): mutations for agent/skill/memory_pool [S042]"
```

---

### Task 9: Auth integration + axum mount; tenant cache invalidation on tenant CRUD

**Files:**
- Create: `crates/simulacra-graphql/src/auth.rs`
- Modify: `crates/simulacra-graphql/src/lib.rs` (export router builder)
- Create: `crates/simulacra-graphql/tests/auth.rs`

- [ ] **Step 1: Implement `auth_middleware` in `src/auth.rs`**

The trait `GraphQLAuthProvider`, `AuthPrincipal`, and `AuthError` were declared in Task 6. Now implement the axum middleware that drives them.

```rust
use std::sync::Arc;
use axum::{extract::Request, middleware::Next, response::Response, http::StatusCode};
use crate::auth::{GraphQLAuthProvider, AuthError};
use crate::context::{GraphQLContext, AuthenticatedPrincipal, TenantResolver};

pub async fn auth_middleware(
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let auth = req.extensions().get::<Arc<dyn GraphQLAuthProvider>>().cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let tenants = req.extensions().get::<TenantResolver>().cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let principal = auth.authenticate(req.headers()).await
        .map_err(|e| match e {
            AuthError::Unauthenticated => StatusCode::UNAUTHORIZED,
            AuthError::Forbidden => StatusCode::FORBIDDEN,
        })?;

    let tenant_id = tenants.resolve(&principal.tenant_namespace).await
        .map_err(|_| StatusCode::FORBIDDEN)?;

    let mut req = req;
    req.extensions_mut().insert(GraphQLContext {
        tenant_id,
        principal: AuthenticatedPrincipal {
            tenant_namespace: principal.tenant_namespace,
            subject: principal.subject,
        },
    });

    Ok(next.run(req).await)
}
```

In `simulacra-server` (Task 10), define a small adapter that implements `GraphQLAuthProvider` by delegating to S031's existing `AuthProvider`:

```rust
// In simulacra-server, NOT in simulacra-graphql
struct GraphQLAuthAdapter(Arc<dyn simulacra_server::auth::AuthProvider>);

#[async_trait::async_trait]
impl simulacra_graphql::auth::GraphQLAuthProvider for GraphQLAuthAdapter {
    async fn authenticate(&self, headers: &http::HeaderMap)
        -> Result<simulacra_graphql::auth::AuthPrincipal, simulacra_graphql::auth::AuthError>
    {
        let p = self.0.authenticate(headers).await
            .map_err(|_| simulacra_graphql::auth::AuthError::Unauthenticated)?;
        Ok(simulacra_graphql::auth::AuthPrincipal {
            tenant_namespace: p.tenant_namespace,
            subject: p.subject,
        })
    }
}
```

- [ ] **Step 3: Export router builder in `src/lib.rs`**

```rust
use std::sync::Arc;
use axum::{Router, routing::post, Extension};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};

pub fn graphql_router(
    schema: schema::SimulacraSchema,
    auth: Arc<dyn auth::GraphQLAuthProvider>,
    tenant_resolver: context::TenantResolver,
) -> Router {
    Router::new()
        .route("/graphql", post(handler))
        .layer(axum::middleware::from_fn(auth::auth_middleware))
        .layer(Extension(schema))
        .layer(Extension(auth))
        .layer(Extension(tenant_resolver))
}

async fn handler(
    Extension(schema): Extension<schema::SimulacraSchema>,
    Extension(ctx): Extension<context::GraphQLContext>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let mut req = req.into_inner();
    req = req.data(ctx);
    schema.execute(req).await.into()
}
```

- [ ] **Step 4: Write `tests/auth.rs`**

```rust
#[tokio::test]
async fn unauthenticated_request_returns_401() {
    // build a Router with a stub AuthProvider that returns Err
    // POST /graphql with no Authorization header
    // assert response status == 401
}

#[tokio::test]
async fn authenticated_request_resolves_tenant_and_executes() {
    // stub AuthProvider returns principal { tenant_namespace: "acme" }
    // catalog seeded with "acme" tenant + an agent
    // POST /graphql with valid Authorization
    // assert query returns the seeded agent
}

#[tokio::test]
async fn principal_for_tenant_a_cannot_query_tenant_b_agent() {
    // seed agent A in tenant "acme", agent B in tenant "evil"
    // authenticate as "acme"
    // query for agent B by id (across tenant boundary)
    // assert: returns null (NotFound mapped to None on get; or error on resolve)
}

#[tokio::test]
async fn principal_for_tenant_a_cannot_mutate_tenant_b_agent() {
    // seed B's agent in "evil"; auth as "acme"
    // updateAgent(id: "<B's id>") → expect error or 0 rows updated
}
```

- [ ] **Step 5: Run tests; iterate**

```bash
cargo test -p simulacra-graphql --test auth
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-graphql/
git commit -m "feat(simulacra-graphql): axum auth middleware + tenant resolution [S042]"
```

---

### Task 10: Rewire `SimulacraEngine` to consume `AgentRepository`

**Files:**
- Modify: `crates/simulacra-server/src/engine.rs`
- Modify: `crates/simulacra-server/Cargo.toml`
- Modify: `crates/simulacra-server/src/lib.rs`
- Modify: `crates/simulacra-server/src/server.rs`
- Create/extend: `crates/simulacra-server/tests/engine_catalog.rs`

- [ ] **Step 1: Add deps**

In `crates/simulacra-server/Cargo.toml`:

```toml
simulacra-catalog.workspace = true
simulacra-graphql.workspace = true
```

- [ ] **Step 2: Extend `SimulacraEngine` constructor**

Modify `engine.rs`:

```rust
pub struct SimulacraEngine {
    config: SimulacraConfig,
    integration_registry: Option<Arc<IntegrationRegistry>>,
    agents: Arc<dyn AgentRepository>,
    skills: Arc<dyn SkillRepository>,
    memory_pools: Arc<dyn MemoryPoolRepository>,
    tenants: Arc<dyn TenantRepository>,
}

impl SimulacraEngine {
    pub fn new(
        config: SimulacraConfig,
        integration_registry: Option<Arc<IntegrationRegistry>>,
        agents: Arc<dyn AgentRepository>,
        skills: Arc<dyn SkillRepository>,
        memory_pools: Arc<dyn MemoryPoolRepository>,
        tenants: Arc<dyn TenantRepository>,
    ) -> Result<Self, EngineError> {
        Ok(Self { config, integration_registry, agents, skills, memory_pools, tenants })
    }
}
```

- [ ] **Step 3: Rewire `spawn_task`**

Replace the `config.agent_types[name]` lookup with:

```rust
async fn spawn_task(
    &self,
    task_manager: &TaskManager,
    description: &str,
    tenant: &TenantConfig,
    agent_type_override: Option<&str>,
    metadata: Value,
    connection_id: Option<String>,
) -> Result<TaskHandle, EngineError> {
    let tenant_row = self.tenants.get_by_namespace(&tenant.namespace).await
        .map_err(|e| EngineError::Tenant(format!("{e}")))?;

    let agent_name = agent_type_override.unwrap_or(&tenant.agent_type);
    let resolved = self.agents.resolve(&tenant_row.id, agent_name).await
        .map_err(|e| match e {
            simulacra_catalog::CatalogError::NotFound(_) => EngineError::AgentNotFound {
                tenant: tenant.namespace.clone(),
                agent: agent_name.to_owned(),
            },
            other => EngineError::Catalog(format!("{other}")),
        })?;

    // ... existing per-task agent construction, but now driven by `resolved.*` ...
    // - VFS composition: include CatalogSkillFs::new(resolved.skills.clone()) at /var/skills/
    // - system prompt from resolved.system_prompt
    // - model from resolved.model
    // - max_turns / max_tokens from resolved
    // - capabilities from resolved.capabilities
    // - memory pool: if resolved.memory_pool.is_some() route SqliteMemoryStore to its store path
}
```

Add `EngineError::AgentNotFound` and `EngineError::Catalog` variants. Add `EngineError::Tenant`.

- [ ] **Step 4: Update server bootstrap (`server.rs`)**

```rust
pub async fn start_server(config: SimulacraConfig, ...) -> Result<...> {
    let catalog_path = config.catalog.db_path.clone();
    let catalog = simulacra_catalog::Catalog::open(&catalog_path)
        .map_err(...)?;

    let agents:      Arc<dyn AgentRepository>      = Arc::new(catalog.agents());
    let skills:      Arc<dyn SkillRepository>      = Arc::new(catalog.skills());
    let memory_pools: Arc<dyn MemoryPoolRepository> = Arc::new(catalog.memory_pools());
    let tenants:     Arc<dyn TenantRepository>     = Arc::new(catalog.tenants());

    // Default tenant ensure
    tenants.get_or_create("default", Some("Default")).await?;

    let engine = Arc::new(SimulacraEngine::new(
        config.clone(), integrations,
        agents.clone(), skills.clone(), memory_pools.clone(), tenants.clone(),
    )?);

    let tenant_resolver = simulacra_graphql::context::TenantResolver::new(tenants.clone());
    let schema = simulacra_graphql::schema::build_schema(agents, skills, memory_pools);

    let app = Router::new()
        .merge(/* existing routes */)
        .merge(simulacra_graphql::graphql_router(schema, auth_provider.clone(), tenant_resolver));

    // ... bind and serve ...
}
```

- [ ] **Step 5: Write integration test `tests/engine_catalog.rs`**

```rust
#[tokio::test]
async fn spawn_task_resolves_agent_from_catalog() {
    let cat = simulacra_catalog::Catalog::open_in_memory().unwrap();
    let tenant = cat.tenants().create("default", None).await.unwrap();
    let s = cat.skills().create(&tenant.id, /* ... */).await.unwrap();
    let agent = cat.agents().create(&tenant.id, /* ... model = "openai/gpt-oss-120b", skill_ids = [s] ... */).await.unwrap();

    let engine = SimulacraEngine::new(
        test_config(), None,
        Arc::new(cat.agents()), Arc::new(cat.skills()),
        Arc::new(cat.memory_pools()), Arc::new(cat.tenants()),
    ).unwrap();

    // Spawn a task and assert it ran with agent.system_prompt as the prompt.
    // (Use existing test infrastructure from S034 integration tests; mock the
    //  provider so the LLM call is deterministic.)
}

#[tokio::test]
async fn spawn_task_with_unknown_agent_returns_agent_not_found() {
    // expect EngineError::AgentNotFound, not panic
}

#[tokio::test]
async fn catalog_skill_visible_at_var_skills_in_running_task() {
    // create an agent with a single skill in catalog; spawn task; have the
    // agent's tool call `list_dir("/var/skills")`; assert it returns
    // ["<name>.md"]
}

#[tokio::test]
async fn catalog_mutation_during_task_does_not_affect_running_task() {
    // start task (long-running); update the agent's prompt; assert the
    // running task's prompt is still the snapshot value
}
```

- [ ] **Step 6: Run tests + mechanical checks**

```bash
cargo test -p simulacra-server --test engine_catalog
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-server/
git commit -m "feat(simulacra-server): SimulacraEngine reads agents from catalog [S042]"
```

---

### Task 11: `simulacra-cli` catalog bootstrap + TOML→DB import (default mode)

**Files:**
- Modify: `crates/simulacra-cli/Cargo.toml`
- Modify: `crates/simulacra-cli/src/lib.rs`
- Modify: `crates/simulacra-config/src/lib.rs` (add `[catalog]`)
- Create: `crates/simulacra-cli/tests/catalog_bootstrap.rs`

- [ ] **Step 1: Add `[catalog]` to `SimulacraConfig`**

In `simulacra-config/src/lib.rs`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CatalogConfig {
    pub db_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SimulacraConfig {
    // ... existing fields ...
    #[serde(default)]
    pub catalog: CatalogConfig,
}
```

Default `db_path` resolution helper:

```rust
impl CatalogConfig {
    pub fn resolved_db_path(&self, state_dir: &Path) -> PathBuf {
        self.db_path.clone().unwrap_or_else(|| state_dir.join("catalog.db"))
    }
}
```

- [ ] **Step 2: Add `simulacra-catalog` dep to `simulacra-cli`**

```toml
# crates/simulacra-cli/Cargo.toml
simulacra-catalog.workspace = true
```

- [ ] **Step 3: TOML→DB import helper**

In `crates/simulacra-cli/src/lib.rs`, add a helper module:

```rust
async fn import_toml_seed(catalog: &Catalog, config: &SimulacraConfig) -> Result<(), CatalogError> {
    let tenants = catalog.tenants();
    let default = tenants.get_or_create("default", Some("Default")).await?;

    // Idempotency check
    let already = catalog.is_seed_applied("toml:agent_types").await?;
    if already { return Ok(()); }

    // Default memory pool from [memory] section
    let memory_pools = catalog.memory_pools();
    let default_pool = memory_pools.get_by_name(&default.id, "default").await.ok();
    let pool_id = match default_pool {
        Some(p) => p.id,
        None => memory_pools.create(&default.id, NewMemoryPool {
            name: "default",
            embedding_model: config.memory.as_ref().and_then(|m| m.embedding_model.as_deref()),
            config: &serde_json::to_value(&config.memory).unwrap_or(serde_json::Value::Null),
        }).await?.id,
    };

    let skills = catalog.skills();
    let agents = catalog.agents();

    for (name, agent_type) in &config.agent_types {
        // Skip if exists (defensive — seeds_applied gate above is the primary)
        if agents.get_by_name(&default.id, name).await.is_ok() { continue; }

        // Build skill_ids by upserting the agent's referenced skills (if any)
        let mut skill_ids = Vec::new();
        for sk in &agent_type.skills {
            // sk has a name + body (or path); upsert into catalog
            let row = match skills.get_by_name(&default.id, &sk.name).await {
                Ok(s) => s,
                Err(_) => skills.create(&default.id, NewSkill {
                    name: &sk.name, description: sk.description.as_deref(),
                    body: &sk.body, metadata: None,
                }).await?,
            };
            skill_ids.push(row.id);
        }

        let caps: Vec<String> = agent_type.capabilities.iter().map(|c| c.to_string()).collect();

        agents.create(&default.id, NewAgent {
            name,
            description: agent_type.description.as_deref(),
            system_prompt: &agent_type.system_prompt,
            model: &agent_type.model,
            max_turns: Some(agent_type.max_turns),
            max_tokens: agent_type.max_tokens,
            memory_pool_id: Some(&pool_id),
            skill_ids: &skill_ids,
            capabilities: &caps,
        }).await?;
    }

    catalog.mark_seed_applied("toml:agent_types").await?;
    tracing::info!(count = config.agent_types.len(), "imported agent_types from simulacra.toml");
    Ok(())
}
```

Also add `is_seed_applied` and `mark_seed_applied` methods to `Catalog`:

```rust
impl Catalog {
    pub async fn is_seed_applied(&self, source: &str) -> Result<bool, CatalogError> { /* SELECT */ }
    pub async fn mark_seed_applied(&self, source: &str) -> Result<(), CatalogError> { /* INSERT */ }
}
```

- [ ] **Step 4: Wire bootstrap in `lib.rs`**

```rust
// In the existing run/bootstrap function, after loading SimulacraConfig:
let state_dir = config.state_dir.clone().unwrap_or_else(|| PathBuf::from("./.simulacra"));
let catalog = Catalog::open(&config.catalog.resolved_db_path(&state_dir))?;
import_toml_seed(&catalog, &config).await?;

let agents = Arc::new(catalog.agents()) as Arc<dyn AgentRepository>;
let skills = Arc::new(catalog.skills()) as Arc<dyn SkillRepository>;
let memory_pools = Arc::new(catalog.memory_pools()) as Arc<dyn MemoryPoolRepository>;
let tenants = Arc::new(catalog.tenants()) as Arc<dyn TenantRepository>;

// Continue with existing agent construction, but pull resolved agent from
// `agents.resolve(&tenant.id, &args.agent_type).await?`
```

- [ ] **Step 5: Write `tests/catalog_bootstrap.rs`**

```rust
#[tokio::test]
async fn fresh_db_imports_agent_types_from_toml() {
    let tmp = TempDir::new().unwrap();
    let cfg = simulacra_config::SimulacraConfig {
        agent_types: hashmap![
            "default".into() => AgentTypeConfig {
                model: "m".into(), system_prompt: "p".into(),
                max_turns: 10, max_tokens: None,
                capabilities: vec![], skills: vec![],
                description: None,
            },
        ],
        catalog: CatalogConfig { db_path: Some(tmp.path().join("c.db")) },
        ..Default::default()
    };
    let catalog = Catalog::open(&cfg.catalog.db_path.clone().unwrap()).unwrap();
    simulacra_cli::bootstrap::import_toml_seed(&catalog, &cfg).await.unwrap();

    let tenants = catalog.tenants();
    let default = tenants.get_by_namespace("default").await.unwrap();
    let agent = catalog.agents().get_by_name(&default.id, "default").await.unwrap();
    assert_eq!(agent.model, "m");
}

#[tokio::test]
async fn re_running_import_is_idempotent() {
    // run once, count agents; mutate TOML to add an agent_type; run again;
    // assert count unchanged (because seeds_applied is true)
}

#[tokio::test]
async fn import_creates_default_memory_pool() {
    // assert "default" memory pool exists after import
}
```

- [ ] **Step 6: Run tests; iterate**

```bash
cargo test -p simulacra-cli --test catalog_bootstrap
```

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-cli/ crates/simulacra-config/
git commit -m "feat(simulacra-cli): catalog bootstrap + TOML import [S042]"
```

---

### Task 12: `simulacra-cli` `--no-catalog` mode

**Files:**
- Modify: `crates/simulacra-cli/src/main.rs` (CLI args)
- Modify: `crates/simulacra-cli/src/lib.rs` (branch on `--no-catalog`)
- Create: `crates/simulacra-cli/tests/no_catalog_mode.rs`

- [ ] **Step 1: Add `--no-catalog` flag**

In `main.rs` (using clap):

```rust
#[derive(clap::Parser)]
struct CliArgs {
    // ... existing args ...
    #[arg(long, help = "Skip the SQLite catalog; resolve agents from simulacra.toml in-memory")]
    no_catalog: bool,
}
```

- [ ] **Step 2: Construct in-memory fixtures from `SimulacraConfig`**

In `lib.rs`, add a helper:

```rust
fn fixtures_from_config(config: &SimulacraConfig) -> Arc<InMemoryFixtures> {
    let mut fx = InMemoryFixtures::default();
    let tenant_id = TenantId("default".into());
    let now = Utc::now();
    fx.tenants.insert(tenant_id.clone(), Tenant {
        id: tenant_id.clone(),
        namespace: "default".into(),
        display_name: Some("Default".into()),
        created_at: now, updated_at: now,
    });

    // Memory pool from [memory]
    if let Some(memory_cfg) = &config.memory {
        let pool_id = MemoryPoolId::new();
        fx.memory_pools.insert(pool_id.clone(), MemoryPool {
            id: pool_id, tenant_id: tenant_id.clone(), name: "default".into(),
            embedding_model: memory_cfg.embedding_model.clone(),
            config: serde_json::to_value(memory_cfg).unwrap_or(serde_json::Value::Null),
            created_at: now, updated_at: now,
        });
    }

    // Skills inlined in agent_types (if any)
    let mut skill_id_by_name: HashMap<String, SkillId> = HashMap::new();
    for at in config.agent_types.values() {
        for sk in &at.skills {
            if !skill_id_by_name.contains_key(&sk.name) {
                let id = SkillId::new();
                skill_id_by_name.insert(sk.name.clone(), id.clone());
                fx.skills.insert(id.clone(), Skill {
                    id, tenant_id: tenant_id.clone(),
                    name: sk.name.clone(),
                    description: sk.description.clone(),
                    body: sk.body.clone(),
                    metadata: None,
                    created_at: now, updated_at: now,
                });
            }
        }
    }

    for (name, at) in &config.agent_types {
        let agent_id = AgentId::new();
        let mp_id = fx.memory_pools.values().next().map(|p| p.id.clone());
        fx.agents.insert(agent_id.clone(), Agent {
            id: agent_id.clone(), tenant_id: tenant_id.clone(),
            name: name.clone(),
            description: at.description.clone(),
            system_prompt: at.system_prompt.clone(),
            model: at.model.clone(),
            max_turns: at.max_turns,
            max_tokens: at.max_tokens,
            memory_pool_id: mp_id,
            created_at: now, updated_at: now,
        });
        let skill_refs = at.skills.iter()
            .filter_map(|s| skill_id_by_name.get(&s.name).cloned())
            .collect();
        fx.agent_skills.insert(agent_id.clone(), skill_refs);
        fx.agent_capabilities.insert(agent_id, at.capabilities.iter().map(|c| c.to_string()).collect());
    }

    Arc::new(fx)
}
```

- [ ] **Step 3: Branch bootstrap on `args.no_catalog`**

```rust
let (agents, skills, memory_pools, tenants): (
    Arc<dyn AgentRepository>, Arc<dyn SkillRepository>,
    Arc<dyn MemoryPoolRepository>, Arc<dyn TenantRepository>,
) = if args.no_catalog {
    let fx = fixtures_from_config(&config);
    (
        Arc::new(MemoryAgentRepository::new(fx.clone())),
        Arc::new(MemorySkillRepository::new(fx.clone())),
        Arc::new(MemoryMemoryPoolRepository::new(fx.clone())),
        Arc::new(MemoryTenantRepository::new(fx)),
    )
} else {
    let catalog = Catalog::open(&config.catalog.resolved_db_path(&state_dir))?;
    import_toml_seed(&catalog, &config).await?;
    (
        Arc::new(catalog.agents()), Arc::new(catalog.skills()),
        Arc::new(catalog.memory_pools()), Arc::new(catalog.tenants()),
    )
};
```

When constructing the per-task VFS for the CLI: in `--no-catalog` mode, do **not** include `CatalogSkillFs` (filesystem-mounted skills only). Otherwise, build `CatalogSkillFs::new(resolved.skills.clone())` and mount at `/var/skills/`.

- [ ] **Step 4: Write `tests/no_catalog_mode.rs`**

```rust
#[tokio::test]
async fn no_catalog_mode_does_not_create_db_file() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("should-not-exist.db");
    let cfg = test_config(/* with [catalog].db_path = db_path */);
    simulacra_cli::run_with_args(CliArgs { no_catalog: true, .. }).await.unwrap();
    assert!(!db_path.exists(), "catalog DB should not have been created");
}

#[tokio::test]
async fn no_catalog_mode_resolves_toml_agent() {
    // build SimulacraConfig with one agent_type
    // run simulacra-cli in --no-catalog mode against a stub provider
    // assert the agent ran with the configured system_prompt
}

#[tokio::test]
async fn no_catalog_mode_mutations_return_readonly() {
    let cfg = test_config_with_one_agent_type();
    let fx = fixtures_from_config(&cfg);
    let repo = MemoryAgentRepository::new(fx);
    let err = repo.delete(&TenantId("default".into()), &AgentId::new()).await.unwrap_err();
    assert!(matches!(err, CatalogError::ReadOnly(_)));
}
```

- [ ] **Step 5: Run tests + mechanical checks**

```bash
cargo test -p simulacra-cli --test no_catalog_mode
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-cli/
git commit -m "feat(simulacra-cli): --no-catalog ephemeral mode [S042]"
```

---

### Task 13: End-to-end smoke — `createAgent` mutation → spawn task → run

**Files:**
- Create: `crates/simulacra-server/tests/graphql_e2e.rs`

This task corresponds to **Phase 3a** in the project protocol — the seam-closing smoke test that proves the catalog is actually wired end-to-end and the spec's "create-an-agent-via-API actually runs" guarantee holds.

- [ ] **Step 1: Spin up a minimal simulacra-server in-process**

```rust
use axum::Router;
use simulacra_catalog::Catalog;
use simulacra_server::{SimulacraEngine, server};
use std::sync::Arc;
use tempfile::TempDir;

async fn boot() -> (Router, Arc<SimulacraEngine>, Catalog, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cat = Catalog::open(&tmp.path().join("catalog.db")).unwrap();
    cat.tenants().get_or_create("default", Some("Default")).await.unwrap();

    let agents:       Arc<dyn AgentRepository>      = Arc::new(cat.agents());
    let skills:       Arc<dyn SkillRepository>      = Arc::new(cat.skills());
    let memory_pools: Arc<dyn MemoryPoolRepository> = Arc::new(cat.memory_pools());
    let tenants:      Arc<dyn TenantRepository>     = Arc::new(cat.tenants());

    let engine = Arc::new(SimulacraEngine::new(
        test_config(),
        None,
        agents.clone(), skills.clone(), memory_pools.clone(), tenants.clone(),
    ).unwrap());

    // Stub auth: every request authenticates as the default tenant.
    struct AlwaysDefault;
    #[async_trait::async_trait]
    impl simulacra_graphql::auth::GraphQLAuthProvider for AlwaysDefault {
        async fn authenticate(&self, _h: &http::HeaderMap) -> Result<_, _> {
            Ok(simulacra_graphql::auth::AuthPrincipal {
                tenant_namespace: "default".into(), subject: "test".into(),
            })
        }
    }

    let resolver = simulacra_graphql::context::TenantResolver::new(tenants.clone());
    let schema = simulacra_graphql::schema::build_schema(agents, skills, memory_pools);
    let gql_router = simulacra_graphql::graphql_router(schema, Arc::new(AlwaysDefault), resolver);

    // For task spawn, mount the existing simulacra-server task routes alongside.
    // Mock the LLM provider via the existing test infrastructure in S034's
    // integration tests (search simulacra-server/tests for `mock_provider` or
    // `recording_provider`).
    let app = Router::new().merge(gql_router) /* + task routes */;

    (app, engine, cat, tmp)
}
```

- [ ] **Step 2: Test: createAgent → spawn task → completion**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_agent_via_graphql_then_run_via_api() {
    let (router, engine, _catalog, _tmp) = boot().await;

    // 1. Mutate: createSkill + createAgent via GraphQL
    let create_skill = json!({"query": r#"
        mutation { createSkill(input: { name: "noop", body: "Just say hi" }) { id } }
    "#});
    let resp = post(&router, "/graphql", &create_skill).await;
    let skill_id = resp["data"]["createSkill"]["id"].as_str().unwrap().to_owned();

    let create_agent = json!({"query": format!(r#"
        mutation {{
            createAgent(input: {{
                name: "e2e",
                systemPrompt: "Reply 'done'.",
                model: "stub",
                skillIds: ["{skill_id}"],
                capabilities: []
            }}) {{ id }}
        }}
    "#)});
    let resp = post(&router, "/graphql", &create_agent).await;
    assert!(resp["errors"].as_array().map_or(true, |a| a.is_empty()));

    // 2. Spawn task via S031 API (or directly via engine.spawn_task in-process)
    let handle = engine.spawn_task(
        &task_manager(),
        "say hi",
        &TenantConfig { namespace: "default".into(), agent_type: "e2e".into(), .. },
        None, json!({}), None,
    ).await.unwrap();

    // 3. Assert task reached terminal Completed state within timeout
    let final_state = wait_for_terminal(&task_manager(), &handle.task_id, Duration::from_secs(10)).await;
    assert_eq!(final_state, TaskState::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_authored_via_graphql_visible_in_running_task_vfs() {
    // The agent's tool calls list_dir("/var/skills") and the result must
    // contain "<the-created-skill-name>.md".
    // Use a stub provider that emits a tool call to a "list_skills" tool,
    // then a final response containing the skill list.
}
```

- [ ] **Step 3: Run + iterate**

```bash
cargo test -p simulacra-server --test graphql_e2e -- --nocapture
```

- [ ] **Step 4: Validate observability via Aniani (per R010)**

After running the e2e test against a local OTLP collector, query Aniani:

- TraceQL: `{ name="simulacra.graphql.request" && resource.service.name="simulacra-server" }` — expect spans with `op_kind=mutation` and `tenant_id="<default tenant ULID>"`.
- TraceQL: `{ name="simulacra.engine.resolve_agent" }` — expect spans with the agent name and id.
- PromQL: `simulacra_graphql_request_duration_count{op_kind="mutation"} > 0`.
- PromQL: `simulacra_catalog_query_duration_count{entity="agent",op="resolve"} > 0`.
- LogQL: `{service_name="simulacra-server"} |= "imported" |= "agent_types"` — expect one info line on first boot.

If any o11y assertion is missing, add the span/metric/log emission and re-run.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-server/tests/graphql_e2e.rs
git commit -m "test(simulacra-server): S042 e2e — createAgent via GraphQL → run [S042]"
```

---

### Final mechanical gate (Phase 3b)

After Tasks 1–13, run the non-negotiable gate:

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

All four must pass before Phase 4 review.

### Phase 4 review (per CLAUDE.md, Gemini removed per memory)

- **4a — copilot GPT-5.4** — review `crates/simulacra-catalog/`, `crates/simulacra-graphql/`, and the `simulacra-server` + `simulacra-cli` modifications against `specs/S042-agent-catalog-graphql.md`, `ARCHITECTURE.md`, and the rules. Classify BLOCKER/WARNING/NIT. Focus: spec compliance, tenant isolation correctness, capability enforcement, hot-reload snapshot semantics, no invented behavior, test coverage of every assertion in S042.
- **4b — Claude sub-agent (holistic)** — does the seam close? Is the `--no-catalog` path actually exercised end-to-end? Are skills shadowed correctly? Any over-engineering relative to the spec?

Fix BLOCKERs; re-run Phase 3b before final commit.

### Phase 5 — final commit

Once Phase 4 passes:

```bash
git log --oneline | head -20   # sanity check
# All per-task commits already landed; final close-out commit if any
# review fixes are still uncommitted:
git commit -m "feat(simulacra-catalog,simulacra-graphql): S042 agent catalog & GraphQL control plane [S042]"
```

Update `specs/S042-agent-catalog-graphql.md` status to `Active` and check off the completed assertions.

```bash
git add specs/S042-agent-catalog-graphql.md SPECS.md
git commit -m "spec(S042): mark Active and check off implemented assertions [S042]"
```
