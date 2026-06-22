# S048 simulacra-frontend v1 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> This plan obeys the protocol in `CLAUDE.md`: Phase 1 (red tests via copilot GPT-5.4 + sub-agent review/reconcile), Phase 2 (green via sub-agents), Phase 3 (e2e + mechanical), Phase 4 (review by copilot GPT-5.4 + Claude sub-agent), Phase 5 (commit). Each task below maps cleanly to one Phase 2 sub-agent dispatch; the per-step TDD discipline is what those sub-agents follow.

**Goal:** Ship the agent-builder UI end-to-end. New `simulacra-frontend` crate ships a no-compile Vue 3 SPA mounted by `simulacra-server` alongside `graphql_router` and the existing REST control plane. Three views — agent list (cards+drawer), agent form (two-column with Save & Run), agent run (dedicated route, activity feed + artifact sidebar). Backend deltas: `NoAuthProvider` for both REST and GraphQL gated by `[server.auth] dev_mode`, mount glue, read-only `GET /api/v1/triggers`, manual-smoke example.

**Architecture:** Single Rust crate that exposes `frontend_router() -> axum::Router` returning a `ServeDir` over its own `assets/` directory. Frontend is native ESM + import maps — Vue and vue-router pulled from `esm.sh`, no bundler. Composables (Vue 3 Composition API) own all I/O; components only consume composables. `simulacra-server::build_router` merges `frontend_router()` and `graphql_router(...)`.

**Tech Stack:** Rust (axum, tower-http with `fs` feature), Vue 3 (no-compile, from esm.sh), vue-router (hash mode), `node --test` for composable unit tests.

---

## Phases

The plan is organized into six phases. Each phase produces working, testable software on its own; later phases depend on earlier ones.

| Phase | Tasks | Lands |
|---|---|---|
| A — Backend foundation | 1–7 | Crate scaffold, NoAuth, mount glue, triggers endpoint, dev_server example |
| B — Frontend foundation | 8–13 | index.html, main.js, vue-router, api/ wrappers, app-shell |
| C — List view | 14–16 | useAgents, agent-list with card grid + drawer |
| D — Form view | 17–23 | All form composables + form components + Save/Save&Run |
| E — Run view | 24–29 | useTaskStream, activity event renderers, artifact sidebar |
| F — Closure | 30–31 | SPECS.md update, manual smoke walkthrough |

After Phase A lands, the spec can be marked Active in SPECS.md (v1 partial — Layer 1).
After Phase F, the spec is fully Active for v1.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `Cargo.toml` (workspace) | Modify | Add `simulacra-frontend` to members; add `tower-http` workspace dep with `fs` feature |
| `crates/simulacra-frontend/Cargo.toml` | Create | Crate manifest |
| `crates/simulacra-frontend/src/lib.rs` | Create | `pub fn frontend_router() -> axum::Router` |
| `crates/simulacra-frontend/assets/index.html` | Create | Vue boot, importmap, root `<div id="app">` |
| `crates/simulacra-frontend/assets/main.js` | Create | Vue app create + vue-router setup |
| `crates/simulacra-frontend/assets/styles.css` | Create | Global tokens, layout primitives |
| `crates/simulacra-frontend/assets/api/graphql.js` | Create | `gql(query, variables)` helper |
| `crates/simulacra-frontend/assets/api/rest.js` | Create | `restJson(path, opts)` + `restMultipart(path, fd)` |
| `crates/simulacra-frontend/assets/api/sse.js` | Create | `openTaskStream(taskId, onEvent)` wrapper |
| `crates/simulacra-frontend/assets/api/identity.js` | Create | Reads dev identity (used to label requests; no auth header in v1) |
| `crates/simulacra-frontend/assets/composables/useAgents.js` | Create | list / get / create / update / saveAndRun |
| `crates/simulacra-frontend/assets/composables/useChannels.js` | Create | list / create |
| `crates/simulacra-frontend/assets/composables/useTools.js` | Create | availableTools query |
| `crates/simulacra-frontend/assets/composables/useSkills.js` | Create | skills query |
| `crates/simulacra-frontend/assets/composables/useAgentFiles.js` | Create | upload + detach |
| `crates/simulacra-frontend/assets/composables/useTriggers.js` | Create | GET /api/v1/triggers |
| `crates/simulacra-frontend/assets/composables/useTaskStream.js` | Create | SSE → reactive events[] + status |
| `crates/simulacra-frontend/assets/composables/useTaskArtifacts.js` | Create | list + download URLs |
| `crates/simulacra-frontend/assets/composables/*.test.mjs` | Create | One test file per composable, run via `node --test` |
| `crates/simulacra-frontend/assets/components/app-shell.js` | Create | Top nav, route outlet, toast container |
| `crates/simulacra-frontend/assets/components/agent-list.js` | Create | Card grid + drawer |
| `crates/simulacra-frontend/assets/components/agent-form.js` | Create | Two-column form |
| `crates/simulacra-frontend/assets/components/agent-run.js` | Create | Activity feed + artifact sidebar |
| `crates/simulacra-frontend/assets/components/pickers/channel-picker.js` | Create | Multi-select w/ inline create |
| `crates/simulacra-frontend/assets/components/pickers/tool-picker.js` | Create | Checkbox list grouped by kind |
| `crates/simulacra-frontend/assets/components/pickers/skill-picker.js` | Create | Single-select dropdown |
| `crates/simulacra-frontend/assets/components/pickers/file-uploader.js` | Create | Multipart upload + detach |
| `crates/simulacra-frontend/assets/components/pickers/trigger-list.js` | Create | Read-only display |
| `crates/simulacra-frontend/assets/components/activity/event-token.js` | Create | Streaming text |
| `crates/simulacra-frontend/assets/components/activity/event-thinking.js` | Create | Collapsible thinking block |
| `crates/simulacra-frontend/assets/components/activity/event-tool-call.js` | Create | Tool invocation w/ args + result |
| `crates/simulacra-frontend/assets/components/activity/event-child.js` | Create | Sub-agent box |
| `crates/simulacra-frontend/assets/components/activity/artifact-sidebar.js` | Create | List + download links |
| `crates/simulacra-frontend/tests/frontend_mount.rs` | Create | Boots router, GET /index.html → 200 |
| `crates/simulacra-server/src/auth.rs` | Modify | Add `NoAuthProvider` |
| `crates/simulacra-server/tests/no_auth_provider.rs` | Create | NoAuthProvider unit tests |
| `crates/simulacra-graphql/src/auth.rs` | Modify | Add `NoAuthGraphQLProvider` |
| `crates/simulacra-graphql/tests/no_auth_graphql_provider.rs` | Create | NoAuthGraphQLProvider unit tests |
| `crates/simulacra-server/src/server.rs` | Modify | Add `schedules` field to `AppState`, mount glue in `build_router`, `GET /api/v1/triggers` handler |
| `crates/simulacra-server/src/api_schema.rs` | Modify | Document the new triggers endpoint |
| `crates/simulacra-server/Cargo.toml` | Modify | Add `simulacra-frontend`, `simulacra-graphql` deps |
| `crates/simulacra-server/tests/triggers_endpoint.rs` | Create | Triggers endpoint behavior |
| `crates/simulacra-server/tests/build_router_e2e.rs` | Create | Full mount integration: frontend + GraphQL + REST live together |
| `crates/simulacra-server/examples/dev_server.rs` | Create | Manual-smoke entrypoint |
| `SPECS.md` | Modify | Mark S048 Active after Phase F |

---

## Phase A — Backend foundation

### Task 1: Scaffold `simulacra-frontend` crate with `ServeDir` mount

**Files:**
- Modify: `Cargo.toml` (workspace)
- Create: `crates/simulacra-frontend/Cargo.toml`
- Create: `crates/simulacra-frontend/src/lib.rs`
- Create: `crates/simulacra-frontend/assets/index.html` (placeholder)
- Create: `crates/simulacra-frontend/tests/frontend_mount.rs`

- [ ] **Step 1: Add workspace dependency**

Modify `Cargo.toml` (root):

```toml
# In [workspace] members =, add:
"crates/simulacra-frontend",

# In [workspace.dependencies], add:
tower-http = { version = "0.5", features = ["cors", "trace", "fs"] }
```

(Note: `simulacra-server` currently takes `tower-http` directly; promoting it to workspace dep so both crates align. After this task, simulacra-server's `Cargo.toml` should use `tower-http.workspace = true` — handled in Task 4.)

- [ ] **Step 2: Create `crates/simulacra-frontend/Cargo.toml`**

```toml
[package]
name = "simulacra-frontend"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
axum.workspace = true
tower-http.workspace = true
tracing.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["full"] }
tower = { workspace = true, features = ["util"] }
hyper.workspace = true
http-body-util = "0.1"
```

- [ ] **Step 3: Write the failing integration test** — `crates/simulacra-frontend/tests/frontend_mount.rs`

```rust
//! Boots the frontend router and verifies static assets are served.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn serves_index_html() {
    let router = simulacra_frontend::frontend_router();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"), "expected text/html, got {ct}");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(body_str.contains("<div id=\"app\""), "index.html should mount #app");
}

#[tokio::test]
async fn returns_404_for_unknown_path() {
    let router = simulacra_frontend::frontend_router();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/does-not-exist.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 4: Run test to verify it fails**

```bash
cargo test -p simulacra-frontend --test frontend_mount
```

Expected: compile error (no `frontend_router` symbol).

- [ ] **Step 5: Create the placeholder asset**

`crates/simulacra-frontend/assets/index.html`:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Simulacra</title>
</head>
<body>
  <div id="app"></div>
</body>
</html>
```

- [ ] **Step 6: Implement `src/lib.rs`**

```rust
//! simulacra-frontend — static SPA assets served via tower-http's ServeDir.
//!
//! Public API: `frontend_router()` returns an `axum::Router` rooted at `/`
//! that serves the crate's `assets/` directory. Mount it via
//! `simulacra-server::build_router` alongside `graphql_router` and the existing
//! REST control plane.

use axum::Router;
use tower_http::services::ServeDir;

/// Build a router that serves the bundled `assets/` directory.
///
/// In v1 this is a runtime read from the source tree's `assets/` directory
/// (relative to `CARGO_MANIFEST_DIR`). v2+ may switch to `include_dir!`
/// embedding behind a cargo feature flag for single-binary distribution.
pub fn frontend_router() -> Router {
    let assets_dir = format!("{}/assets", env!("CARGO_MANIFEST_DIR"));
    Router::new().fallback_service(ServeDir::new(assets_dir))
}
```

- [ ] **Step 7: Run test to verify it passes**

```bash
cargo test -p simulacra-frontend --test frontend_mount
```

Expected: both tests pass.

- [ ] **Step 8: Mechanical checks**

```bash
cargo build -p simulacra-frontend
cargo clippy -p simulacra-frontend --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml crates/simulacra-frontend
git commit -m "feat(simulacra-frontend): scaffold crate with ServeDir mount [S048]"
```

---

### Task 2: `NoAuthProvider` in `simulacra-server`

**Files:**
- Modify: `crates/simulacra-server/src/auth.rs`
- Create: `crates/simulacra-server/tests/no_auth_provider.rs`

- [ ] **Step 1: Write the failing test** — `crates/simulacra-server/tests/no_auth_provider.rs`

```rust
use simulacra_server::{AuthProvider, Credentials, NoAuthProvider};
use simulacra_types::TenantId;

#[tokio::test]
async fn returns_configured_identity_for_any_credentials() {
    let provider = NoAuthProvider::new("dev@local", "default");
    let identity = provider
        .authenticate(&Credentials::None)
        .await
        .expect("NoAuthProvider should never fail");
    assert_eq!(identity.subject, "dev@local");
    assert_eq!(identity.tenant_namespace.as_deref(), Some("default"));
}

#[tokio::test]
async fn ignores_bearer_token() {
    let provider = NoAuthProvider::new("dev@local", "default");
    let identity = provider
        .authenticate(&Credentials::Bearer("anything".to_string()))
        .await
        .expect("NoAuthProvider should never fail");
    assert_eq!(identity.subject, "dev@local");
}

#[tokio::test]
async fn ignores_api_key() {
    let provider = NoAuthProvider::new("dev@local", "default");
    let identity = provider
        .authenticate(&Credentials::ApiKey("anything".to_string()))
        .await
        .expect("NoAuthProvider should never fail");
    assert_eq!(identity.subject, "dev@local");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p simulacra-server --test no_auth_provider
```

Expected: compile error (no `NoAuthProvider` symbol).

- [ ] **Step 3: Implement `NoAuthProvider`** — append to `crates/simulacra-server/src/auth.rs`

```rust
/// Auth provider that returns a fixed identity regardless of credentials.
///
/// **Only** for development. Wire in via `[server.auth] dev_mode = true`
/// in `simulacra.toml` (or in the `examples/dev_server.rs` smoke entrypoint).
/// Production deploys must use `OidcAuthProvider`, `ApiKeyAuthProvider`,
/// or `CompositeAuthProvider`.
pub struct NoAuthProvider {
    subject: String,
    tenant_namespace: String,
}

impl NoAuthProvider {
    pub fn new(subject: impl Into<String>, tenant_namespace: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            tenant_namespace: tenant_namespace.into(),
        }
    }
}

#[async_trait::async_trait]
impl AuthProvider for NoAuthProvider {
    async fn authenticate(&self, _credentials: &Credentials) -> Result<Identity, AuthError> {
        Ok(Identity {
            subject: self.subject.clone(),
            tenant_namespace: Some(self.tenant_namespace.clone()),
            scopes: vec![],
        })
    }
}
```

Then re-export from `crates/simulacra-server/src/lib.rs`:

```rust
pub use auth::{
    ApiKeyAuthProvider, ApiKeyEntry, AuthError, AuthProvider, CompositeAuthProvider, Credentials,
    Identity, NoAuthProvider, OidcAuthProvider, OidcConfig,
};
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p simulacra-server --test no_auth_provider
```

Expected: all three tests pass.

- [ ] **Step 5: Mechanical checks**

```bash
cargo build -p simulacra-server
cargo clippy -p simulacra-server --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-server/src/auth.rs crates/simulacra-server/src/lib.rs crates/simulacra-server/tests/no_auth_provider.rs
git commit -m "feat(simulacra-server): add NoAuthProvider for dev-mode auth bypass [S048]"
```

---

### Task 3: `NoAuthGraphQLProvider` in `simulacra-graphql`

**Files:**
- Modify: `crates/simulacra-graphql/src/auth.rs`
- Create: `crates/simulacra-graphql/tests/no_auth_graphql_provider.rs`

- [ ] **Step 1: Write the failing test** — `crates/simulacra-graphql/tests/no_auth_graphql_provider.rs`

```rust
use axum::http::HeaderMap;
use simulacra_graphql::auth::{GraphQLAuthProvider, NoAuthGraphQLProvider};

#[tokio::test]
async fn returns_configured_principal_for_empty_headers() {
    let provider = NoAuthGraphQLProvider::new("dev@local", "default");
    let principal = provider.authenticate(&HeaderMap::new()).await.unwrap();
    assert_eq!(principal.subject, "dev@local");
    assert_eq!(principal.tenant_namespace, "default");
}

#[tokio::test]
async fn ignores_authorization_header() {
    let provider = NoAuthGraphQLProvider::new("dev@local", "default");
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer anything".parse().unwrap());
    let principal = provider.authenticate(&headers).await.unwrap();
    assert_eq!(principal.subject, "dev@local");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p simulacra-graphql --test no_auth_graphql_provider
```

Expected: compile error.

- [ ] **Step 3: Implement `NoAuthGraphQLProvider`** — append to `crates/simulacra-graphql/src/auth.rs`

```rust
/// GraphQL auth provider that returns a fixed principal regardless of headers.
/// Dev-mode only — see `simulacra-server::NoAuthProvider` for the REST counterpart.
pub struct NoAuthGraphQLProvider {
    subject: String,
    tenant_namespace: String,
}

impl NoAuthGraphQLProvider {
    pub fn new(subject: impl Into<String>, tenant_namespace: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            tenant_namespace: tenant_namespace.into(),
        }
    }
}

#[async_trait::async_trait]
impl GraphQLAuthProvider for NoAuthGraphQLProvider {
    async fn authenticate(&self, _headers: &HeaderMap) -> Result<AuthPrincipal, AuthError> {
        Ok(AuthPrincipal {
            tenant_namespace: self.tenant_namespace.clone(),
            subject: self.subject.clone(),
        })
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p simulacra-graphql --test no_auth_graphql_provider
```

- [ ] **Step 5: Mechanical checks**

```bash
cargo build -p simulacra-graphql
cargo clippy -p simulacra-graphql --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-graphql/src/auth.rs crates/simulacra-graphql/tests/no_auth_graphql_provider.rs
git commit -m "feat(simulacra-graphql): add NoAuthGraphQLProvider for dev-mode auth bypass [S048]"
```

---

### Task 4: Mount `frontend_router()` and `graphql_router()` in `simulacra-server::build_router`

**Files:**
- Modify: `crates/simulacra-server/Cargo.toml`
- Modify: `crates/simulacra-server/src/server.rs`
- Create: `crates/simulacra-server/tests/build_router_e2e.rs`

- [ ] **Step 1: Write the failing integration test** — `crates/simulacra-server/tests/build_router_e2e.rs`

```rust
//! End-to-end: build_router mounts frontend, GraphQL, and REST together.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

use simulacra_server::{
    AppState, NoAuthProvider, build_router,
};
use simulacra_server::auth::AuthProvider;
use simulacra_server::tenant::TenantResolver;
use simulacra_server::task::TaskManager;

fn make_state() -> AppState {
    let task_manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::default());
    let auth: Arc<dyn AuthProvider> =
        Arc::new(NoAuthProvider::new("dev@local", "default"));
    AppState::new(task_manager, resolver, auth)
}

#[tokio::test]
async fn serves_frontend_index_at_root() {
    let router = build_router(make_state(), vec![]);
    let response = router
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"));
}

#[tokio::test]
async fn graphql_endpoint_is_reachable() {
    let router = build_router(make_state(), vec![]);
    let body = serde_json::json!({ "query": "{ __typename }" }).to_string();
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/graphql")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    // GraphQL requires a tenant resolver path; for this smoke we accept any 2xx.
    assert!(
        response.status().is_success() || response.status() == StatusCode::FORBIDDEN,
        "expected GraphQL endpoint to be wired (got {})",
        response.status()
    );
}

#[tokio::test]
async fn rest_schema_endpoint_is_not_shadowed_by_static() {
    let router = build_router(make_state(), vec![]);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/schema")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("application/json"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p simulacra-server --test build_router_e2e
```

Expected: compile failure (simulacra-server doesn't depend on simulacra-frontend or simulacra-graphql yet).

- [ ] **Step 3: Add the deps to `crates/simulacra-server/Cargo.toml`**

```toml
[dependencies]
simulacra-frontend.workspace = true
simulacra-graphql.workspace = true
# (existing deps unchanged)
```

Add to `[workspace.dependencies]` in root `Cargo.toml`:

```toml
simulacra-frontend = { path = "crates/simulacra-frontend" }
simulacra-graphql = { path = "crates/simulacra-graphql" }
```

(`simulacra-graphql` is already a path dep elsewhere; verify and consolidate.)

- [ ] **Step 4: Modify `build_router` in `crates/simulacra-server/src/server.rs`**

Find the existing `pub fn build_router` (around line 2058). Add merges at the end of the chain, before the `.with_state` (or wherever the final return is). Pattern:

```rust
pub fn build_router(state: AppState, adapters: Vec<Box<dyn ProtocolAdapter>>) -> Router {
    let mut router = Router::new()
        // ... existing routes (unchanged) ...
        ;

    // ── adapters (existing block, unchanged) ──
    for adapter in adapters { /* ... */ }

    // ── webhook handlers (existing, unchanged) ──
    for webhook_config in &state.webhooks { /* ... */ }

    // S048 — mount GraphQL gateway. Must precede the static fallback so
    // POST /graphql reaches async-graphql, not ServeDir.
    let graphql_auth: std::sync::Arc<dyn simulacra_graphql::auth::GraphQLAuthProvider> =
        std::sync::Arc::new(simulacra_graphql::auth::NoAuthGraphQLProvider::new(
            "dev@local",
            "default",
        ));
    let graphql_tenants = simulacra_graphql::context::TenantResolver::new(
        std::sync::Arc::clone(&state.engine.tenants()),
    );
    let graphql_ctx = simulacra_graphql::context::GraphQLContext::new(
        std::sync::Arc::clone(&state.engine.agents()),
        // pass the other repos exactly the same way graphql_router currently expects;
        // see graphql/src/lib.rs::graphql_router signature for the canonical call.
    );
    router = router.merge(simulacra_graphql::graphql_router(
        graphql_auth,
        graphql_tenants,
        graphql_ctx,
    ));

    // S048 — mount frontend (static SPA). Always last — it has a `/` fallback
    // that would shadow other routes if mounted first.
    router = router.merge(simulacra_frontend::frontend_router());

    router.with_state(state)
}
```

> **Note for the implementer:** `graphql_router(...)`'s exact signature lives in `crates/simulacra-graphql/src/lib.rs`. The wiring above is structural — match parameters exactly to that function's current signature. If `AppState` doesn't expose the catalog repos for GraphQL today, add the necessary getters (or pass the repos through `AppState` if they aren't there yet) — Phase A spec compliance requires GraphQL to be reachable.

- [ ] **Step 5: Run the test to verify it passes**

```bash
cargo test -p simulacra-server --test build_router_e2e
```

Expected: all three tests pass.

- [ ] **Step 6: Run the full simulacra-server test suite to verify no regression**

```bash
cargo test -p simulacra-server
```

Expected: pre-existing tests still pass.

- [ ] **Step 7: Mechanical checks**

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/simulacra-server/Cargo.toml crates/simulacra-server/src/server.rs crates/simulacra-server/tests/build_router_e2e.rs
git commit -m "feat(simulacra-server): mount frontend_router and graphql_router in build_router [S048]"
```

---

### Task 5: Add `schedules` to `AppState` (preparatory for triggers endpoint)

**Files:**
- Modify: `crates/simulacra-server/src/server.rs`

This is a small, mechanical change with no test of its own — Task 6's triggers endpoint exercises the new field. Doing it as its own task keeps the diff readable and the commit isolated.

- [ ] **Step 1: Add the `schedules` field to `AppState`**

In `crates/simulacra-server/src/server.rs`, find the `pub struct AppState` definition (around line 110) and add:

```rust
/// Schedule configurations — surfaced via `GET /api/v1/triggers`.
/// The actual `Scheduler` runs separately as a background task; this
/// list is the source of truth for *what schedules exist*.
pub schedules: Vec<crate::scheduler::ScheduleConfig>,
```

- [ ] **Step 2: Update every constructor**

Each `impl AppState { ... new / with_engine / with_memory / with_webhooks / with_engine_and_webhooks }` constructor must initialize `schedules: vec![]`. Add a sibling builder `with_schedules` that takes `(webhooks, schedules)` together (signature mirrors `with_webhooks`):

```rust
pub fn with_triggers(
    task_manager: Arc<TaskManager>,
    resolver: Arc<TenantResolver>,
    auth: Arc<dyn AuthProvider>,
    webhooks: Vec<WebhookConfig>,
    schedules: Vec<crate::scheduler::ScheduleConfig>,
) -> Self {
    let mut state = Self::with_webhooks(task_manager, resolver, auth, webhooks);
    state.schedules = schedules;
    state
}
```

- [ ] **Step 3: Verify nothing regresses**

```bash
cargo build --workspace
cargo test -p simulacra-server
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-server/src/server.rs
git commit -m "feat(simulacra-server): add schedules to AppState for triggers endpoint [S048]"
```

---

### Task 6: `GET /api/v1/triggers` endpoint

**Files:**
- Modify: `crates/simulacra-server/src/server.rs`
- Modify: `crates/simulacra-server/src/api_schema.rs`
- Create: `crates/simulacra-server/tests/triggers_endpoint.rs`

- [ ] **Step 1: Write the failing test** — `crates/simulacra-server/tests/triggers_endpoint.rs`

```rust
//! Tests for GET /api/v1/triggers.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

use simulacra_server::{AppState, NoAuthProvider, build_router};
use simulacra_server::auth::AuthProvider;
use simulacra_server::scheduler::{MissedPolicy, ScheduleConfig};
use simulacra_server::tenant::TenantResolver;
use simulacra_server::task::TaskManager;
use simulacra_server::webhook::WebhookConfig;

fn make_state(
    webhooks: Vec<WebhookConfig>,
    schedules: Vec<ScheduleConfig>,
) -> AppState {
    let task_manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::default());
    let auth: Arc<dyn AuthProvider> =
        Arc::new(NoAuthProvider::new("dev@local", "default"));
    AppState::with_triggers(task_manager, resolver, auth, webhooks, schedules)
}

fn webhook(path: &str, agent: &str, hmac: bool) -> WebhookConfig {
    WebhookConfig {
        path: path.to_string(),
        agent_type: agent.to_string(),
        tenant: "default".to_string(),
        hmac_secret: if hmac { Some("secret".into()) } else { None },
        // adjust to actual WebhookConfig shape — fill in any other required fields with sensible defaults
        ..Default::default()
    }
}

fn schedule(cron: &str, agent: &str) -> ScheduleConfig {
    ScheduleConfig {
        cron: cron.to_string(),
        agent_type: agent.to_string(),
        tenant: "default".to_string(),
        missed_policy: MissedPolicy::Skip,
        // adjust to actual ScheduleConfig shape
        ..Default::default()
    }
}

#[tokio::test]
async fn returns_all_triggers_for_tenant_when_no_filter() {
    let state = make_state(
        vec![webhook("/hooks/zendesk", "triage", true)],
        vec![schedule("0 9 * * 1", "weekly-report")],
    );
    let router = build_router(state, vec![]);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/triggers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["webhooks"].as_array().unwrap().len(), 1);
    assert_eq!(v["schedules"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn filters_by_agent_query_param() {
    let state = make_state(
        vec![
            webhook("/hooks/a", "triage", false),
            webhook("/hooks/b", "other", false),
        ],
        vec![
            schedule("0 9 * * 1", "triage"),
            schedule("0 10 * * 1", "other"),
        ],
    );
    let router = build_router(state, vec![]);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/triggers?agent=triage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    let webhooks = v["webhooks"].as_array().unwrap();
    assert_eq!(webhooks.len(), 1);
    assert_eq!(webhooks[0]["agent_type"], "triage");
    let schedules = v["schedules"].as_array().unwrap();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0]["agent_type"], "triage");
}

#[tokio::test]
async fn webhook_response_includes_hmac_presence_flag_not_secret() {
    let state = make_state(
        vec![webhook("/hooks/secret", "triage", true)],
        vec![],
    );
    let router = build_router(state, vec![]);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/triggers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    let webhook = &v["webhooks"][0];
    assert_eq!(webhook["hmac"], Value::Bool(true));
    assert!(
        webhook.get("hmac_secret").is_none(),
        "secret must NEVER appear in the response"
    );
}

#[tokio::test]
async fn cross_tenant_triggers_are_filtered_out() {
    let mut webhook_a = webhook("/hooks/mine", "triage", false);
    let mut webhook_b = webhook("/hooks/other-tenant", "triage", false);
    webhook_b.tenant = "different-tenant".to_string();
    let state = make_state(vec![webhook_a, webhook_b], vec![]);
    let router = build_router(state, vec![]);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/triggers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["webhooks"].as_array().unwrap().len(), 1);
    assert_eq!(v["webhooks"][0]["path"], "/hooks/mine");
}
```

> **Implementer note:** the precise field set on `WebhookConfig` and `ScheduleConfig` may need additional defaults beyond what's shown above. If the structs don't `derive(Default)`, add it (gated on a `#[cfg(test)]` or as a permanent change — both are fine). The test's intent is the assertion shape, not the constructor exact form.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p simulacra-server --test triggers_endpoint
```

Expected: compile failure (route doesn't exist).

- [ ] **Step 3: Add the handler to `crates/simulacra-server/src/server.rs`**

After existing handlers, before `build_router`:

```rust
/// GET /api/v1/triggers[?agent=:agent_type]
///
/// Returns the configured webhooks and schedule entries for the caller's
/// tenant. Read-only; mirrors `[[webhooks]]` / `[[schedules]]` in `simulacra.toml`.
async fn list_triggers(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<TriggersQuery>,
) -> Response {
    let credentials = extract_credentials(&headers);
    let identity = match state.auth.authenticate(&credentials).await {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": {"code": "unauthorized", "message": e.to_string()}})),
            )
                .into_response();
        }
    };
    let tenant = match state.resolver.resolve(&identity) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"ok": false, "error": {"code": "forbidden", "message": e.to_string()}})),
            )
                .into_response();
        }
    };
    let tenant_str = tenant.namespace();

    let webhooks: Vec<Value> = state
        .webhooks
        .iter()
        .filter(|w| w.tenant == tenant_str)
        .filter(|w| match &params.agent {
            Some(a) => &w.agent_type == a,
            None => true,
        })
        .map(|w| {
            json!({
                "path": w.path,
                "agent_type": w.agent_type,
                "hmac": w.hmac_secret.is_some(),
            })
        })
        .collect();

    let schedules: Vec<Value> = state
        .schedules
        .iter()
        .filter(|s| s.tenant == tenant_str)
        .filter(|s| match &params.agent {
            Some(a) => &s.agent_type == a,
            None => true,
        })
        .map(|s| {
            json!({
                "cron": s.cron,
                "agent_type": s.agent_type,
                "missed_policy": format!("{:?}", s.missed_policy).to_lowercase(),
            })
        })
        .collect();

    (StatusCode::OK, Json(json!({ "webhooks": webhooks, "schedules": schedules }))).into_response()
}

#[derive(Debug, Deserialize)]
struct TriggersQuery {
    agent: Option<String>,
}
```

- [ ] **Step 4: Wire the route in `build_router`**

In `pub fn build_router(...)`, add to the existing `Router::new()` chain (with the other `/api/v1/*` routes):

```rust
.route("/api/v1/triggers", get(list_triggers))
```

- [ ] **Step 5: Document in `crates/simulacra-server/src/api_schema.rs`**

Append a description of the new route to the JSON returned by the existing `api_schema()` function (look for the existing routes; follow the same shape — path, method, brief description).

- [ ] **Step 6: Run the test to verify it passes**

```bash
cargo test -p simulacra-server --test triggers_endpoint
```

Expected: all four tests pass.

- [ ] **Step 7: Mechanical checks**

```bash
cargo build --workspace
cargo clippy -p simulacra-server --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 8: Commit**

```bash
git add crates/simulacra-server/src/server.rs crates/simulacra-server/src/api_schema.rs crates/simulacra-server/tests/triggers_endpoint.rs
git commit -m "feat(simulacra-server): GET /api/v1/triggers read-only endpoint [S048]"
```

---

### Task 7: `examples/dev_server.rs` for manual smoke

**Files:**
- Create: `crates/simulacra-server/examples/dev_server.rs`

This isn't TDD — it's a runnable demo that the rest of v1 needs for hand-validation. No automated test; running it is the test.

- [ ] **Step 1: Create the example**

`crates/simulacra-server/examples/dev_server.rs`:

```rust
//! S048 dev server — boots simulacra-server with NoAuth + frontend + GraphQL.
//!
//! Usage:
//!   cargo run -p simulacra-server --example dev_server
//!
//! Then open http://localhost:8080 in a browser.
//!
//! What this wires together:
//!   - NoAuthProvider for both REST and GraphQL (dev_mode-equivalent)
//!   - In-memory catalog seeded with one example agent so the list view
//!     has something to show
//!   - Empty webhook/schedule lists
//!   - frontend_router() at /, graphql_router() at /graphql, REST at /api/v1/*

use std::sync::Arc;

use simulacra_server::{
    AppState, NoAuthProvider, ServerConfig, build_router, start_server,
};
use simulacra_server::auth::AuthProvider;
use simulacra_server::tenant::TenantResolver;
use simulacra_server::task::TaskManager;
use simulacra_catalog::Catalog;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let catalog = Catalog::open_in_memory()?;
    // Seed one agent so the list view isn't empty on first open.
    seed_default_tenant_and_agent(&catalog).await?;

    let task_manager = Arc::new(TaskManager::new());
    let resolver = Arc::new(TenantResolver::default());
    let auth: Arc<dyn AuthProvider> =
        Arc::new(NoAuthProvider::new("dev@local", "default"));

    let state = AppState::with_triggers(
        task_manager,
        resolver,
        auth,
        vec![], // webhooks
        vec![], // schedules
    );

    let router = build_router(state, vec![]);

    let config = ServerConfig {
        host: "127.0.0.1".to_string(),
        port: 8080,
    };

    println!("dev_server listening on http://{}:{}", config.host, config.port);
    println!("  - UI:      http://{}:{}/", config.host, config.port);
    println!("  - GraphQL: http://{}:{}/graphql", config.host, config.port);
    println!("  - REST:    http://{}:{}/api/v1/*", config.host, config.port);

    start_server(config, router).await?;
    Ok(())
}

async fn seed_default_tenant_and_agent(catalog: &Catalog) -> Result<(), Box<dyn std::error::Error>> {
    use simulacra_catalog::repo::{TenantRepository, AgentRepository};
    use simulacra_catalog::models::{NewTenant, NewAgent};

    let tenants = catalog.tenants();
    tenants.upsert(NewTenant {
        namespace: "default".into(),
        display_name: "Default".into(),
    }).await?;
    let tenant_id = tenants.get_by_namespace("default").await?.id;

    let agents = catalog.agents();
    agents.create(&tenant_id, NewAgent {
        name: "example-agent".into(),
        system_prompt: "You are a friendly demo agent.".into(),
        capabilities: vec!["shell:exec".into()],
        skill_ids: &[],
        channel_ids: &[],
    }).await?;

    Ok(())
}
```

> **Implementer note:** match `NewTenant`, `NewAgent`, and method signatures to whatever simulacra-catalog currently exposes. If a method isn't there or has a different shape, adapt — the goal is "boot a server with one seeded agent."

- [ ] **Step 2: Run it**

```bash
cargo run -p simulacra-server --example dev_server
```

Expected:
- Server prints the three URLs and listens on port 8080
- `curl http://127.0.0.1:8080/` returns the placeholder `index.html` (200, text/html)
- `curl http://127.0.0.1:8080/api/v1/schema` returns the schema JSON
- `curl -X POST http://127.0.0.1:8080/graphql -H 'content-type: application/json' -d '{"query":"{ __typename }"}'` returns a GraphQL response
- Ctrl-C stops the server

- [ ] **Step 3: Mechanical checks**

```bash
cargo build -p simulacra-server --examples
cargo clippy -p simulacra-server --examples -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-server/examples/dev_server.rs
git commit -m "feat(simulacra-server): dev_server example for S048 manual smoke [S048]"
```

---

**Phase A acceptance:** Tasks 1–7 complete. Backend can serve the placeholder UI + GraphQL + REST + triggers endpoint with NoAuth, all behind a single binary launch via `cargo run -p simulacra-server --example dev_server`. Mark S048 Active in SPECS.md (note "Layer 1 (backend) only" — full v1 still pending Phase B–F).

---

## Phase B — Frontend foundation

Phase B replaces the placeholder `index.html` with the real Vue app, sets up `vue-router`, the API wrappers, and the app shell.

### Task 8: Real `index.html` with importmap + Vue boot

**Files:**
- Modify: `crates/simulacra-frontend/assets/index.html`

- [ ] **Step 1: Replace the placeholder**

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>Simulacra</title>
  <link rel="stylesheet" href="/styles.css">
  <script type="importmap">
  {
    "imports": {
      "vue": "https://esm.sh/vue@3.4.21/dist/vue.esm-browser.js",
      "vue-router": "https://esm.sh/vue-router@4.3.0?deps=vue@3.4.21"
    }
  }
  </script>
</head>
<body>
  <div id="app"></div>
  <script type="module" src="/main.js"></script>
</body>
</html>
```

- [ ] **Step 2: Verify the existing mount test still passes**

```bash
cargo test -p simulacra-frontend --test frontend_mount
```

Expected: pass (the test only checks `<div id="app"`, which is still present).

- [ ] **Step 3: Commit**

```bash
git add crates/simulacra-frontend/assets/index.html
git commit -m "feat(simulacra-frontend): real index.html with Vue importmap [S048]"
```

---

### Task 9: `main.js` + minimal `vue-router` with route stubs

**Files:**
- Create: `crates/simulacra-frontend/assets/main.js`
- Create: `crates/simulacra-frontend/assets/components/app-shell.js` (stub)
- Create: `crates/simulacra-frontend/assets/styles.css` (minimal base)

- [ ] **Step 1: Create `main.js`**

```js
// main.js — Vue app entry. vue-router in hash mode.
import { createApp, defineAsyncComponent } from 'vue';
import { createRouter, createWebHashHistory } from 'vue-router';

import AppShell from '/components/app-shell.js';

const routes = [
  { path: '/', component: defineAsyncComponent(() => import('/components/agent-list.js')) },
  { path: '/agents/new', component: defineAsyncComponent(() => import('/components/agent-form.js')) },
  { path: '/agents/:id', component: defineAsyncComponent(() => import('/components/agent-form.js')), props: true },
  { path: '/agents/:id/run/:taskId', component: defineAsyncComponent(() => import('/components/agent-run.js')), props: true },
];

const router = createRouter({
  history: createWebHashHistory(),
  routes,
});

createApp(AppShell).use(router).mount('#app');
```

- [ ] **Step 2: Create the stub `app-shell.js`**

```js
// app-shell.js — top nav + <router-view> outlet + global toast.
import { ref, h, defineComponent } from 'vue';

export const toasts = ref([]);

export function showToast(message, kind = 'error', timeoutMs = 5000) {
  const id = Math.random().toString(36).slice(2);
  toasts.value.push({ id, message, kind });
  setTimeout(() => {
    toasts.value = toasts.value.filter(t => t.id !== id);
  }, timeoutMs);
}

export default defineComponent({
  name: 'AppShell',
  template: `
    <div class="app">
      <header class="app__header">
        <strong>simulacra</strong>
        <nav>
          <router-link to="/">Agents</router-link>
        </nav>
      </header>
      <main class="app__main">
        <router-view />
      </main>
      <div class="toasts">
        <div v-for="t in toasts" :key="t.id" :class="['toast', 'toast--' + t.kind]">
          {{ t.message }}
        </div>
      </div>
    </div>
  `,
  setup() {
    return { toasts };
  },
});
```

- [ ] **Step 3: Create `styles.css`**

```css
:root {
  --bg: #fff;
  --fg: #1f2328;
  --muted: #56636e;
  --border: #d0d7de;
  --hover: #f6f8fa;
  --primary: #0969da;
  --danger: #cf222e;
  --success: #1a7f37;
  --space-1: 4px;
  --space-2: 8px;
  --space-3: 12px;
  --space-4: 16px;
  --space-6: 24px;
  --radius: 6px;
}

body, html, #app { margin: 0; padding: 0; height: 100%; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; color: var(--fg); background: var(--bg); }

.app { display: flex; flex-direction: column; height: 100%; }
.app__header { display: flex; gap: var(--space-6); align-items: center; padding: var(--space-3) var(--space-4); border-bottom: 1px solid var(--border); }
.app__header nav a { color: var(--fg); text-decoration: none; padding: var(--space-1) var(--space-2); }
.app__header nav a.router-link-active { font-weight: 600; }
.app__main { flex: 1; overflow: auto; padding: var(--space-4); }

.toasts { position: fixed; top: var(--space-4); right: var(--space-4); display: flex; flex-direction: column; gap: var(--space-2); z-index: 1000; }
.toast { padding: var(--space-2) var(--space-3); border-radius: var(--radius); background: var(--bg); border: 1px solid var(--border); box-shadow: 0 2px 8px rgba(0,0,0,0.08); }
.toast--error { border-color: var(--danger); color: var(--danger); }
.toast--info { border-color: var(--primary); color: var(--primary); }

button { padding: var(--space-2) var(--space-3); border: 1px solid var(--border); background: var(--bg); border-radius: var(--radius); cursor: pointer; }
button:hover { background: var(--hover); }
button.primary { background: var(--primary); color: #fff; border-color: var(--primary); }

input, select, textarea { padding: var(--space-2); border: 1px solid var(--border); border-radius: var(--radius); font: inherit; box-sizing: border-box; }
.label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted); margin-bottom: var(--space-1); }
```

- [ ] **Step 4: Verify in the browser via dev_server example**

Run `cargo run -p simulacra-server --example dev_server` and open http://127.0.0.1:8080. Expect: blank "agents" page (the agent-list component import will fail since it doesn't exist yet — note the console error and continue). Page header should render.

> The console error is expected — `defineAsyncComponent` lazy-imports modules that don't exist yet. Phase C will add them.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/main.js crates/simulacra-frontend/assets/components/app-shell.js crates/simulacra-frontend/assets/styles.css
git commit -m "feat(simulacra-frontend): main.js, app-shell, base styles [S048]"
```

---

### Task 10: `api/graphql.js` + `api/rest.js` + tests

**Files:**
- Create: `crates/simulacra-frontend/assets/api/graphql.js`
- Create: `crates/simulacra-frontend/assets/api/rest.js`
- Create: `crates/simulacra-frontend/assets/api/identity.js`
- Create: `crates/simulacra-frontend/assets/api/graphql.test.mjs`
- Create: `crates/simulacra-frontend/assets/api/rest.test.mjs`

- [ ] **Step 1: Write the failing tests**

`graphql.test.mjs`:

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { gql, GraphQLError } from './graphql.js';

function mockFetch(response) {
  return async (url, opts) => {
    mockFetch.lastCall = { url, opts };
    return {
      ok: response.ok ?? true,
      status: response.status ?? 200,
      json: async () => response.body,
    };
  };
}

test('gql posts query + variables to /graphql', async () => {
  globalThis.fetch = mockFetch({ body: { data: { __typename: 'Query' } } });
  const data = await gql('{ __typename }', { x: 1 });
  assert.equal(data.__typename, 'Query');
  assert.equal(mockFetch.lastCall.url, '/graphql');
  assert.equal(mockFetch.lastCall.opts.method, 'POST');
  const body = JSON.parse(mockFetch.lastCall.opts.body);
  assert.equal(body.query, '{ __typename }');
  assert.deepEqual(body.variables, { x: 1 });
});

test('gql throws GraphQLError on errors[]', async () => {
  globalThis.fetch = mockFetch({ body: { errors: [{ message: 'boom' }] } });
  await assert.rejects(() => gql('{ x }'), e => e instanceof GraphQLError && e.message.includes('boom'));
});

test('gql throws on non-OK status', async () => {
  globalThis.fetch = mockFetch({ ok: false, status: 500, body: {} });
  await assert.rejects(() => gql('{ x }'));
});
```

`rest.test.mjs`:

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { restJson, restMultipart } from './rest.js';

function mockFetch(response) {
  return async (url, opts) => {
    mockFetch.lastCall = { url, opts };
    return {
      ok: response.ok ?? true,
      status: response.status ?? 200,
      headers: { get: (k) => k === 'content-type' ? 'application/json' : null },
      json: async () => response.body,
    };
  };
}

test('restJson GETs by default', async () => {
  globalThis.fetch = mockFetch({ body: { ok: true } });
  const out = await restJson('/api/v1/foo');
  assert.equal(out.ok, true);
  assert.equal(mockFetch.lastCall.opts.method ?? 'GET', 'GET');
});

test('restJson POSTs with body', async () => {
  globalThis.fetch = mockFetch({ body: { ok: true } });
  await restJson('/api/v1/foo', { method: 'POST', body: { a: 1 } });
  const opts = mockFetch.lastCall.opts;
  assert.equal(opts.method, 'POST');
  assert.equal(opts.headers['content-type'], 'application/json');
  assert.equal(JSON.parse(opts.body).a, 1);
});

test('restJson throws on non-OK', async () => {
  globalThis.fetch = mockFetch({ ok: false, status: 404, body: { error: 'nope' } });
  await assert.rejects(() => restJson('/api/v1/foo'));
});

test('restMultipart sends FormData', async () => {
  globalThis.fetch = mockFetch({ body: { id: 'abc' } });
  const fd = new FormData();
  fd.append('file', new Blob(['hi']), 'hi.txt');
  const out = await restMultipart('/api/v1/upload', fd);
  assert.equal(out.id, 'abc');
  assert.equal(mockFetch.lastCall.opts.method, 'POST');
  assert.ok(mockFetch.lastCall.opts.body instanceof FormData);
});
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd crates/simulacra-frontend/assets/api && node --test
```

Expected: failures (modules don't exist).

- [ ] **Step 3: Implement `identity.js`**

```js
// identity.js — dev identity used to label requests in v1.
// In v2+ this becomes the seam for attaching real bearer tokens.
export const DEV_IDENTITY = {
  subject: 'dev@local',
  tenant: 'default',
};
```

- [ ] **Step 4: Implement `graphql.js`**

```js
// graphql.js — POST queries/mutations to /graphql.
// Throws GraphQLError on `errors[]` in the response. Throws plain Error
// on transport / non-OK status.

export class GraphQLError extends Error {
  constructor(messages, raw) {
    super(messages.join('; '));
    this.name = 'GraphQLError';
    this.errors = raw;
  }
}

export async function gql(query, variables) {
  const response = await fetch('/graphql', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ query, variables: variables ?? null }),
  });
  if (!response.ok) {
    throw new Error(`GraphQL HTTP ${response.status}`);
  }
  const payload = await response.json();
  if (payload.errors && payload.errors.length > 0) {
    throw new GraphQLError(payload.errors.map(e => e.message), payload.errors);
  }
  return payload.data;
}
```

- [ ] **Step 5: Implement `rest.js`**

```js
// rest.js — fetch wrappers for /api/v1/*.
// `restJson` for JSON request/response; `restMultipart` for file uploads.

export async function restJson(path, opts = {}) {
  const headers = { ...(opts.headers ?? {}) };
  let body = opts.body;
  if (body !== undefined && typeof body !== 'string' && !(body instanceof FormData)) {
    headers['content-type'] = 'application/json';
    body = JSON.stringify(body);
  }
  const response = await fetch(path, {
    method: opts.method ?? 'GET',
    headers,
    body,
  });
  if (!response.ok) {
    throw new Error(`REST ${path} → HTTP ${response.status}`);
  }
  const ct = response.headers.get('content-type') ?? '';
  if (ct.startsWith('application/json')) {
    return await response.json();
  }
  return await response.text();
}

export async function restMultipart(path, formData) {
  const response = await fetch(path, { method: 'POST', body: formData });
  if (!response.ok) {
    throw new Error(`REST ${path} → HTTP ${response.status}`);
  }
  return await response.json();
}
```

- [ ] **Step 6: Run tests to verify they pass**

```bash
cd crates/simulacra-frontend/assets/api && node --test
```

Expected: all 7 tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-frontend/assets/api
git commit -m "feat(simulacra-frontend): graphql.js + rest.js + identity.js with tests [S048]"
```

---

### Task 11: `api/sse.js` for task event streams

**Files:**
- Create: `crates/simulacra-frontend/assets/api/sse.js`
- Create: `crates/simulacra-frontend/assets/api/sse.test.mjs`

- [ ] **Step 1: Write the failing test**

`sse.test.mjs`:

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';

// Mock EventSource — node doesn't have one.
class MockEventSource {
  constructor(url) {
    this.url = url;
    this.listeners = {};
    MockEventSource.lastInstance = this;
  }
  addEventListener(type, fn) { this.listeners[type] = fn; }
  close() { this.closed = true; }
  emit(type, data) {
    const listener = this.listeners[type];
    if (listener) listener({ data: JSON.stringify(data) });
  }
}

globalThis.EventSource = MockEventSource;

const { openTaskStream } = await import('./sse.js');

test('opens EventSource at /api/v1/tasks/:id/events', () => {
  const handle = openTaskStream('task_abc');
  assert.equal(MockEventSource.lastInstance.url, '/api/v1/tasks/task_abc/events');
  handle.close();
});

test('parses message events into onEvent callbacks', () => {
  let received = [];
  const handle = openTaskStream('task_abc', (event) => received.push(event));
  MockEventSource.lastInstance.emit('message', { type: 'token', text: 'hi' });
  MockEventSource.lastInstance.emit('message', { type: 'task_complete' });
  assert.equal(received.length, 2);
  assert.equal(received[0].type, 'token');
  assert.equal(received[1].type, 'task_complete');
  handle.close();
});

test('close() shuts down the EventSource', () => {
  const handle = openTaskStream('task_abc');
  handle.close();
  assert.equal(MockEventSource.lastInstance.closed, true);
});

test('onError fires when EventSource dispatches error', () => {
  let error;
  const handle = openTaskStream('task_abc', () => {}, (err) => { error = err; });
  MockEventSource.lastInstance.listeners.error?.({});
  assert.ok(error);
  handle.close();
});
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd crates/simulacra-frontend/assets/api && node --test sse.test.mjs
```

Expected: import failure.

- [ ] **Step 3: Implement `sse.js`**

```js
// sse.js — wraps EventSource for /api/v1/tasks/:id/events.
//
// onEvent(event)  — called for each parsed message
// onError(err)    — called if the underlying EventSource errors
// Returns { close() }.

export function openTaskStream(taskId, onEvent = () => {}, onError = () => {}) {
  const url = `/api/v1/tasks/${taskId}/events`;
  const source = new EventSource(url);

  source.addEventListener('message', (e) => {
    try {
      const data = JSON.parse(e.data);
      onEvent(data);
    } catch (parseErr) {
      onError(parseErr);
    }
  });
  source.addEventListener('error', (e) => {
    onError(e);
  });

  return { close: () => source.close() };
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd crates/simulacra-frontend/assets/api && node --test sse.test.mjs
```

Expected: all 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/api/sse.js crates/simulacra-frontend/assets/api/sse.test.mjs
git commit -m "feat(simulacra-frontend): sse.js task-event-stream wrapper [S048]"
```

---

**Phase B acceptance:** Tasks 8–11 complete. The frontend has a router, an app shell with toast support, base styles, and the three API wrappers. Browsing to `/` lazy-loads the missing `agent-list` component (console error expected until Phase C).

---

## Phase C — List view

### Task 12: `useAgents` composable (list + get only)

**Files:**
- Create: `crates/simulacra-frontend/assets/composables/useAgents.js`
- Create: `crates/simulacra-frontend/assets/composables/useAgents.test.mjs`

This task only implements `list()` and `get()`. `create()`, `update()`, and `saveAndRun()` arrive in Task 19 alongside the form.

- [ ] **Step 1: Write the failing tests**

`useAgents.test.mjs`:

```js
import { test, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

let mockResponse;
let lastCall;
globalThis.fetch = async (url, opts) => {
  lastCall = { url, opts };
  return {
    ok: true,
    status: 200,
    headers: { get: () => 'application/json' },
    json: async () => mockResponse,
  };
};

const { useAgents } = await import('./useAgents.js');

beforeEach(() => { lastCall = null; mockResponse = null; });

test('list() issues agents query', async () => {
  mockResponse = { data: { agents: { edges: [{ node: { id: '1', name: 'a' } }] } } };
  const { list, agents, loading, error } = useAgents();
  await list();
  assert.equal(loading.value, false);
  assert.equal(error.value, null);
  assert.equal(agents.value.length, 1);
  assert.equal(agents.value[0].name, 'a');
  const body = JSON.parse(lastCall.opts.body);
  assert.match(body.query, /agents/);
});

test('list() captures errors into error ref without throwing', async () => {
  mockResponse = { errors: [{ message: 'denied' }] };
  const { list, error, agents } = useAgents();
  await list();
  assert.match(error.value.message, /denied/);
  assert.deepEqual(agents.value, []);
});

test('get(id) fetches a single agent', async () => {
  mockResponse = { data: { agent: { id: '1', name: 'a', systemPrompt: 'be helpful' } } };
  const { get } = useAgents();
  const agent = await get('1');
  assert.equal(agent.name, 'a');
  const body = JSON.parse(lastCall.opts.body);
  assert.match(body.query, /agent\(/);
  assert.deepEqual(body.variables, { id: '1' });
});
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useAgents.test.mjs
```

Expected: import failure.

- [ ] **Step 3: Implement `useAgents.js` (list + get)**

```js
// useAgents.js — list / get composable. create/update/saveAndRun are added
// in Task 19 alongside the form.
import { ref } from 'vue';
import { gql } from '/api/graphql.js';

const LIST_QUERY = `
  query {
    agents(page: { first: 100 }) {
      edges {
        node {
          id name updatedAt
          channels { id name kind }
          tools { id name kind }
          skills { id name }
          files { id name size }
        }
      }
    }
  }
`;

const GET_QUERY = `
  query($id: ID!) {
    agent(id: $id) {
      id name systemPrompt updatedAt
      capabilities
      channels { id name kind config }
      tools { id name kind capabilities }
      skills { id name }
      files { id name size }
    }
  }
`;

export function useAgents() {
  const agents = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true;
    error.value = null;
    try {
      const data = await gql(LIST_QUERY);
      agents.value = data.agents.edges.map(e => e.node);
    } catch (e) {
      error.value = e;
      agents.value = [];
    } finally {
      loading.value = false;
    }
  }

  async function get(id) {
    loading.value = true;
    error.value = null;
    try {
      const data = await gql(GET_QUERY, { id });
      return data.agent;
    } catch (e) {
      error.value = e;
      return null;
    } finally {
      loading.value = false;
    }
  }

  return { agents, loading, error, list, get };
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useAgents.test.mjs
```

Expected: all 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useAgents.js crates/simulacra-frontend/assets/composables/useAgents.test.mjs
git commit -m "feat(simulacra-frontend): useAgents composable (list + get) [S048]"
```

---

### Task 13: `agent-list` component — card grid + drawer

**Files:**
- Create: `crates/simulacra-frontend/assets/components/agent-list.js`

This component is large enough that we test it manually rather than with a unit test in v1. Its data path is `useAgents` (already tested).

- [ ] **Step 1: Implement the component**

```js
// agent-list.js — Card grid of agents with a drawer that opens to show
// composition. Drawer state is local; URL stays at "/".
import { defineComponent, ref, onMounted, computed } from 'vue';
import { useAgents } from '/composables/useAgents.js';

export default defineComponent({
  name: 'AgentList',
  template: `
    <div class="agent-list">
      <div class="agent-list__header">
        <input v-model="filter" placeholder="filter…" class="agent-list__filter" />
        <router-link to="/agents/new"><button class="primary">+ New agent</button></router-link>
      </div>
      <div v-if="loading">Loading…</div>
      <div v-else-if="error">Failed to load: {{ error.message }}</div>
      <div v-else class="agent-list__grid">
        <div
          v-for="agent in filteredAgents"
          :key="agent.id"
          class="agent-card"
          @click="openDrawer(agent.id)"
        >
          <strong>{{ agent.name }}</strong>
          <div class="agent-card__meta">
            {{ (agent.tools || []).length }} tools · {{ formatRelative(agent.updatedAt) }}
          </div>
          <div class="agent-card__chips">
            <span v-for="ch in agent.channels" :key="ch.id" class="chip">{{ ch.name }}</span>
          </div>
        </div>
      </div>

      <aside v-if="drawerAgent" class="drawer" @click.self="closeDrawer">
        <div class="drawer__panel">
          <header class="drawer__header">
            <h2>{{ drawerAgent.name }}</h2>
            <div class="drawer__actions">
              <router-link :to="'/agents/' + drawerAgent.id"><button>Edit</button></router-link>
              <button class="primary" @click="runAgent(drawerAgent.id)">▶ Run</button>
              <button @click="closeDrawer">×</button>
            </div>
          </header>
          <dl class="drawer__details">
            <dt>Channels</dt><dd>{{ (drawerAgent.channels||[]).map(c=>c.name).join(', ') || '—' }}</dd>
            <dt>Tools</dt><dd>{{ (drawerAgent.tools||[]).map(t=>t.name).join(', ') || '—' }}</dd>
            <dt>Skills</dt><dd>{{ (drawerAgent.skills||[]).map(s=>s.name).join(', ') || '—' }}</dd>
            <dt>Files</dt><dd>{{ (drawerAgent.files||[]).map(f=>f.name).join(', ') || '—' }}</dd>
            <dt>Capabilities</dt><dd>{{ (drawerAgent.capabilities||[]).join(', ') || '—' }}</dd>
            <dt>Prompt</dt><dd><pre>{{ drawerAgent.systemPrompt }}</pre></dd>
          </dl>
        </div>
      </aside>
    </div>
  `,
  setup() {
    const { agents, loading, error, list, get } = useAgents();
    const filter = ref('');
    const drawerAgent = ref(null);

    onMounted(() => list());

    const filteredAgents = computed(() => {
      const q = filter.value.toLowerCase();
      if (!q) return agents.value;
      return agents.value.filter(a => a.name.toLowerCase().includes(q));
    });

    async function openDrawer(id) {
      drawerAgent.value = await get(id);
    }
    function closeDrawer() { drawerAgent.value = null; }

    async function runAgent(id) {
      // Phase E will wire the run flow. v1 navigates to a placeholder.
      // For now, navigate to the run route with a placeholder taskId.
      window.location.hash = `#/agents/${id}/run/pending`;
    }

    return { agents, loading, error, filter, filteredAgents, drawerAgent, openDrawer, closeDrawer, runAgent };
  },
});

function formatRelative(iso) {
  if (!iso) return 'unknown';
  const ms = Date.now() - new Date(iso).getTime();
  const days = Math.round(ms / 86_400_000);
  if (days < 1) return 'today';
  if (days === 1) return '1d ago';
  if (days < 30) return `${days}d ago`;
  return new Date(iso).toLocaleDateString();
}
```

- [ ] **Step 2: Add list-specific CSS to `styles.css`**

Append:

```css
.agent-list__header { display: flex; justify-content: space-between; margin-bottom: var(--space-4); align-items: center; }
.agent-list__filter { width: 240px; }
.agent-list__grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: var(--space-3); }
.agent-card { border: 1px solid var(--border); border-radius: var(--radius); padding: var(--space-3); cursor: pointer; }
.agent-card:hover { background: var(--hover); }
.agent-card__meta { font-size: 11px; color: var(--muted); margin-top: var(--space-1); }
.agent-card__chips { margin-top: var(--space-2); display: flex; flex-wrap: wrap; gap: var(--space-1); }
.chip { font-size: 11px; padding: 2px 6px; background: var(--hover); border-radius: 10px; }

.drawer { position: fixed; inset: 0; background: rgba(0,0,0,0.2); display: flex; justify-content: flex-end; z-index: 20; }
.drawer__panel { background: var(--bg); width: 55%; height: 100%; padding: var(--space-4); overflow: auto; box-shadow: -6px 0 12px rgba(0,0,0,0.06); }
.drawer__header { display: flex; justify-content: space-between; align-items: flex-start; margin-bottom: var(--space-4); }
.drawer__actions { display: flex; gap: var(--space-1); }
.drawer__details { display: grid; grid-template-columns: 110px 1fr; gap: var(--space-1) var(--space-3); font-size: 13px; }
.drawer__details dt { color: var(--muted); font-weight: normal; }
.drawer__details dd { margin: 0; }
.drawer__details pre { font-family: ui-monospace, monospace; font-size: 11px; background: var(--hover); padding: var(--space-2); border-radius: 4px; white-space: pre-wrap; }
```

- [ ] **Step 3: Manual smoke**

Run `cargo run -p simulacra-server --example dev_server`, open http://127.0.0.1:8080. Expect:
- The seeded "example-agent" card appears in the grid
- Clicking the card opens the drawer with all composition fields populated
- Edit and Run buttons render (clicking them is stubbed for now)

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-frontend/assets/components/agent-list.js crates/simulacra-frontend/assets/styles.css
git commit -m "feat(simulacra-frontend): agent-list component with card grid + drawer [S048]"
```

---

**Phase C acceptance:** Tasks 12–13 complete. Use case 2 (list + view composition) works end-to-end against the dev_server.

---

## Phase D — Form view

The form is the largest single chunk. It pulls in five pickers, five composables, and one big component.

### Task 14: `useChannels` composable

**Files:**
- Create: `crates/simulacra-frontend/assets/composables/useChannels.js`
- Create: `crates/simulacra-frontend/assets/composables/useChannels.test.mjs`

- [ ] **Step 1: Write the failing tests**

```js
import { test, beforeEach } from 'node:test';
import assert from 'node:assert/strict';

let mockResponse, lastCall;
globalThis.fetch = async (url, opts) => {
  lastCall = { url, opts };
  return { ok: true, status: 200, headers: { get: () => 'application/json' }, json: async () => mockResponse };
};
const { useChannels } = await import('./useChannels.js');
beforeEach(() => { mockResponse = null; lastCall = null; });

test('list() returns channels from edges', async () => {
  mockResponse = { data: { channels: { edges: [{ node: { id: '1', name: '#support', kind: 'SLACK' } }] } } };
  const { list, channels } = useChannels();
  await list();
  assert.equal(channels.value.length, 1);
  assert.equal(channels.value[0].name, '#support');
});

test('create({ name, kind, config }) issues createChannel mutation and refreshes list', async () => {
  // First call: createChannel mutation. Second call: list refresh.
  let calls = 0;
  globalThis.fetch = async (url, opts) => {
    calls++;
    if (calls === 1) {
      const body = JSON.parse(opts.body);
      assert.match(body.query, /createChannel/);
      return { ok: true, status: 200, headers: { get: () => 'application/json' }, json: async () => ({ data: { createChannel: { id: '2', name: 'new', kind: 'SLACK' } } }) };
    }
    return { ok: true, status: 200, headers: { get: () => 'application/json' }, json: async () => ({ data: { channels: { edges: [{ node: { id: '2', name: 'new', kind: 'SLACK' } }] } } }) };
  };
  const { create, channels } = useChannels();
  const channel = await create({ name: 'new', kind: 'SLACK', config: {} });
  assert.equal(channel.id, '2');
  assert.equal(channels.value[0].id, '2');
});
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useChannels.test.mjs
```

- [ ] **Step 3: Implement `useChannels.js`**

```js
import { ref } from 'vue';
import { gql } from '/api/graphql.js';

const LIST = `query { channels(page:{first:100}) { edges { node { id name kind config } } } }`;
const CREATE = `mutation($input: CreateChannelInput!) { createChannel(input: $input) { id name kind config } }`;

export function useChannels() {
  const channels = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true; error.value = null;
    try {
      const data = await gql(LIST);
      channels.value = data.channels.edges.map(e => e.node);
    } catch (e) { error.value = e; channels.value = []; }
    finally { loading.value = false; }
  }

  async function create(input) {
    error.value = null;
    try {
      const data = await gql(CREATE, { input });
      await list();
      return data.createChannel;
    } catch (e) { error.value = e; throw e; }
  }

  return { channels, loading, error, list, create };
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useChannels.test.mjs
```

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useChannels.js crates/simulacra-frontend/assets/composables/useChannels.test.mjs
git commit -m "feat(simulacra-frontend): useChannels composable [S048]"
```

---

### Task 15: `useTools` and `useSkills` composables

**Files:**
- Create: `crates/simulacra-frontend/assets/composables/useTools.js` (+ `.test.mjs`)
- Create: `crates/simulacra-frontend/assets/composables/useSkills.js` (+ `.test.mjs`)

These two are read-only and structurally identical. Implementing both as one task.

- [ ] **Step 1: Write the failing tests**

`useTools.test.mjs`:

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';
let mockResponse;
globalThis.fetch = async () => ({ ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => mockResponse });
const { useTools } = await import('./useTools.js');

test('list() returns tools', async () => {
  mockResponse = { data: { availableTools: [{ id: 'shell:exec', name: 'shell', kind: 'shell', capabilities: ['shell:exec'] }] } };
  const { list, tools } = useTools();
  await list();
  assert.equal(tools.value.length, 1);
  assert.equal(tools.value[0].id, 'shell:exec');
});
```

`useSkills.test.mjs`:

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';
let mockResponse;
globalThis.fetch = async () => ({ ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => mockResponse });
const { useSkills } = await import('./useSkills.js');

test('list() returns skills', async () => {
  mockResponse = { data: { skills: { edges: [{ node: { id: '1', name: 'triage' } }] } } };
  const { list, skills } = useSkills();
  await list();
  assert.equal(skills.value.length, 1);
  assert.equal(skills.value[0].name, 'triage');
});
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useTools.test.mjs useSkills.test.mjs
```

- [ ] **Step 3: Implement `useTools.js`**

```js
import { ref } from 'vue';
import { gql } from '/api/graphql.js';

const LIST = `query { availableTools { id name kind capabilities } }`;

export function useTools() {
  const tools = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true; error.value = null;
    try {
      const data = await gql(LIST);
      tools.value = data.availableTools;
    } catch (e) { error.value = e; tools.value = []; }
    finally { loading.value = false; }
  }

  return { tools, loading, error, list };
}
```

- [ ] **Step 4: Implement `useSkills.js`**

```js
import { ref } from 'vue';
import { gql } from '/api/graphql.js';

const LIST = `query { skills(page:{first:200}) { edges { node { id name } } } }`;

export function useSkills() {
  const skills = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true; error.value = null;
    try {
      const data = await gql(LIST);
      skills.value = data.skills.edges.map(e => e.node);
    } catch (e) { error.value = e; skills.value = []; }
    finally { loading.value = false; }
  }

  return { skills, loading, error, list };
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useTools.test.mjs useSkills.test.mjs
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useTools.js crates/simulacra-frontend/assets/composables/useTools.test.mjs crates/simulacra-frontend/assets/composables/useSkills.js crates/simulacra-frontend/assets/composables/useSkills.test.mjs
git commit -m "feat(simulacra-frontend): useTools + useSkills composables [S048]"
```

---

### Task 16: `useAgentFiles` composable (multipart upload + detach)

**Files:**
- Create: `crates/simulacra-frontend/assets/composables/useAgentFiles.js` (+ `.test.mjs`)

- [ ] **Step 1: Write the failing tests**

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';

let calls = [];
globalThis.fetch = async (url, opts) => {
  calls.push({ url, opts });
  if (url.endsWith('/files') && opts.method === 'POST') {
    return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ id: 'f1', name: 'r.pdf', size: 100 }) };
  }
  // detach via GraphQL
  return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { detachAgentFile: true } }) };
};
const { useAgentFiles } = await import('./useAgentFiles.js');

test('upload(agentId, file) POSTs multipart to /api/v1/agents/:id/files', async () => {
  calls = [];
  const { upload } = useAgentFiles('agent_1');
  const blob = new Blob(['hello']);
  const file = new File([blob], 'r.pdf');
  const result = await upload(file);
  assert.equal(result.id, 'f1');
  assert.equal(calls[0].url, '/api/v1/agents/agent_1/files');
  assert.equal(calls[0].opts.method, 'POST');
  assert.ok(calls[0].opts.body instanceof FormData);
});

test('detach(fileId) issues detachAgentFile mutation', async () => {
  calls = [];
  const { detach } = useAgentFiles('agent_1');
  const ok = await detach('f1');
  assert.equal(ok, true);
  const body = JSON.parse(calls[0].opts.body);
  assert.match(body.query, /detachAgentFile/);
  assert.equal(body.variables.agentId, 'agent_1');
  assert.equal(body.variables.fileId, 'f1');
});
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useAgentFiles.test.mjs
```

- [ ] **Step 3: Implement `useAgentFiles.js`**

```js
import { ref } from 'vue';
import { gql } from '/api/graphql.js';
import { restMultipart } from '/api/rest.js';

const DETACH = `mutation($agentId: ID!, $fileId: ID!) { detachAgentFile(agentId: $agentId, fileId: $fileId) }`;

export function useAgentFiles(agentId) {
  const error = ref(null);
  const uploading = ref(false);

  async function upload(file) {
    uploading.value = true;
    error.value = null;
    try {
      const fd = new FormData();
      fd.append('file', file, file.name);
      return await restMultipart(`/api/v1/agents/${agentId}/files`, fd);
    } catch (e) { error.value = e; throw e; }
    finally { uploading.value = false; }
  }

  async function detach(fileId) {
    error.value = null;
    try {
      const data = await gql(DETACH, { agentId, fileId });
      return data.detachAgentFile;
    } catch (e) { error.value = e; throw e; }
  }

  return { uploading, error, upload, detach };
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useAgentFiles.test.mjs
```

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useAgentFiles.js crates/simulacra-frontend/assets/composables/useAgentFiles.test.mjs
git commit -m "feat(simulacra-frontend): useAgentFiles composable (upload + detach) [S048]"
```

---

### Task 17: `useTriggers` composable

**Files:**
- Create: `crates/simulacra-frontend/assets/composables/useTriggers.js` (+ `.test.mjs`)

- [ ] **Step 1: Write the failing test**

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';
let mockResponse, lastCall;
globalThis.fetch = async (url) => {
  lastCall = url;
  return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => mockResponse };
};
const { useTriggers } = await import('./useTriggers.js');

test('list(agentId) hits /api/v1/triggers?agent=:id and exposes shape', async () => {
  mockResponse = { webhooks: [{ path: '/hooks/x', agent_type: 'a', hmac: true }], schedules: [{ cron: '0 9 * * 1', agent_type: 'a', missed_policy: 'skip' }] };
  const { list, webhooks, schedules } = useTriggers();
  await list('a');
  assert.equal(lastCall, '/api/v1/triggers?agent=a');
  assert.equal(webhooks.value.length, 1);
  assert.equal(schedules.value.length, 1);
});

test('list() with no agentId hits /api/v1/triggers (no filter)', async () => {
  mockResponse = { webhooks: [], schedules: [] };
  const { list } = useTriggers();
  await list();
  assert.equal(lastCall, '/api/v1/triggers');
});
```

- [ ] **Step 2: Run test to verify it fails**

- [ ] **Step 3: Implement `useTriggers.js`**

```js
import { ref } from 'vue';
import { restJson } from '/api/rest.js';

export function useTriggers() {
  const webhooks = ref([]);
  const schedules = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list(agentId) {
    loading.value = true; error.value = null;
    try {
      const path = agentId ? `/api/v1/triggers?agent=${encodeURIComponent(agentId)}` : '/api/v1/triggers';
      const data = await restJson(path);
      webhooks.value = data.webhooks || [];
      schedules.value = data.schedules || [];
    } catch (e) { error.value = e; webhooks.value = []; schedules.value = []; }
    finally { loading.value = false; }
  }

  return { webhooks, schedules, loading, error, list };
}
```

- [ ] **Step 4: Run test to verify it passes**

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useTriggers.js crates/simulacra-frontend/assets/composables/useTriggers.test.mjs
git commit -m "feat(simulacra-frontend): useTriggers composable [S048]"
```

---

### Task 18: Pickers — channel, tool, skill, file-uploader, trigger-list

**Files:**
- Create: `crates/simulacra-frontend/assets/components/pickers/channel-picker.js`
- Create: `crates/simulacra-frontend/assets/components/pickers/tool-picker.js`
- Create: `crates/simulacra-frontend/assets/components/pickers/skill-picker.js`
- Create: `crates/simulacra-frontend/assets/components/pickers/file-uploader.js`
- Create: `crates/simulacra-frontend/assets/components/pickers/trigger-list.js`

These are presentational components. Each takes a `v-model` (selected ids) plus the relevant composable's data. No unit tests in v1 — they're exercised by manual smoke after Task 19.

- [ ] **Step 1: `channel-picker.js`**

```js
import { defineComponent, onMounted, ref, computed } from 'vue';
import { useChannels } from '/composables/useChannels.js';

export default defineComponent({
  name: 'ChannelPicker',
  props: { modelValue: { type: Array, default: () => [] } },
  emits: ['update:modelValue'],
  template: `
    <div class="picker">
      <div class="picker__chips">
        <span v-for="id in modelValue" :key="id" class="chip">
          {{ nameFor(id) }} <button class="chip__x" @click="remove(id)">×</button>
        </span>
      </div>
      <select @change="addFromSelect($event)">
        <option value="">+ add channel…</option>
        <option v-for="ch in available" :key="ch.id" :value="ch.id">{{ ch.name }} ({{ ch.kind }})</option>
        <option value="__new__">+ create new…</option>
      </select>
      <div v-if="creating" class="picker__inline-create">
        <input v-model="newName" placeholder="name" />
        <select v-model="newKind">
          <option>SLACK</option><option>TEAMS</option><option>EMAIL</option><option>WEBHOOK</option><option>MANUAL</option>
        </select>
        <button @click="createChannel">create</button>
        <button @click="creating = false">cancel</button>
      </div>
    </div>
  `,
  setup(props, { emit }) {
    const { channels, list, create } = useChannels();
    const creating = ref(false);
    const newName = ref('');
    const newKind = ref('SLACK');

    onMounted(() => list());

    const available = computed(() => channels.value.filter(c => !props.modelValue.includes(c.id)));

    function nameFor(id) {
      const c = channels.value.find(c => c.id === id);
      return c ? c.name : id;
    }
    function remove(id) {
      emit('update:modelValue', props.modelValue.filter(x => x !== id));
    }
    function addFromSelect(e) {
      const v = e.target.value;
      e.target.value = '';
      if (!v) return;
      if (v === '__new__') { creating.value = true; return; }
      emit('update:modelValue', [...props.modelValue, v]);
    }
    async function createChannel() {
      const ch = await create({ name: newName.value, kind: newKind.value, config: {} });
      newName.value = '';
      creating.value = false;
      emit('update:modelValue', [...props.modelValue, ch.id]);
    }
    return { available, nameFor, remove, addFromSelect, creating, newName, newKind, createChannel };
  },
});
```

- [ ] **Step 2: `tool-picker.js`**

```js
import { defineComponent, onMounted, computed } from 'vue';
import { useTools } from '/composables/useTools.js';

export default defineComponent({
  name: 'ToolPicker',
  props: { modelValue: { type: Array, default: () => [] } },
  emits: ['update:modelValue'],
  template: `
    <div class="picker tool-picker">
      <div v-for="(group, kind) in groupedTools" :key="kind">
        <div class="label">{{ kind }}</div>
        <label v-for="tool in group" :key="tool.id" class="tool-picker__row">
          <input
            type="checkbox"
            :checked="modelValue.includes(tool.id)"
            @change="toggle(tool.id, $event.target.checked)"
          />
          {{ tool.name }}
        </label>
      </div>
    </div>
  `,
  setup(props, { emit }) {
    const { tools, list } = useTools();
    onMounted(() => list());
    const groupedTools = computed(() => {
      const groups = {};
      for (const t of tools.value) {
        groups[t.kind] = groups[t.kind] || [];
        groups[t.kind].push(t);
      }
      return groups;
    });
    function toggle(id, on) {
      const next = on
        ? [...props.modelValue, id]
        : props.modelValue.filter(x => x !== id);
      emit('update:modelValue', next);
    }
    return { groupedTools, toggle };
  },
});
```

- [ ] **Step 3: `skill-picker.js`**

```js
import { defineComponent, onMounted } from 'vue';
import { useSkills } from '/composables/useSkills.js';

export default defineComponent({
  name: 'SkillPicker',
  props: { modelValue: { type: String, default: null } },
  emits: ['update:modelValue'],
  template: `
    <select :value="modelValue" @change="$emit('update:modelValue', $event.target.value || null)">
      <option value="">— none —</option>
      <option v-for="s in skills" :key="s.id" :value="s.id">{{ s.name }}</option>
    </select>
  `,
  setup() {
    const { skills, list } = useSkills();
    onMounted(() => list());
    return { skills };
  },
});
```

- [ ] **Step 4: `file-uploader.js`**

```js
import { defineComponent, ref } from 'vue';
import { useAgentFiles } from '/composables/useAgentFiles.js';

export default defineComponent({
  name: 'FileUploader',
  props: {
    agentId: { type: String, default: null },
    files: { type: Array, default: () => [] },
  },
  emits: ['change'],
  template: `
    <div class="picker">
      <div v-if="!agentId" class="picker__hint">Save the agent first to upload files.</div>
      <div v-else>
        <div v-for="f in files" :key="f.id" class="file-row">
          <span>{{ f.name }} <span class="dim">({{ f.size }} bytes)</span></span>
          <button @click="onDetach(f.id)">remove</button>
        </div>
        <input type="file" @change="onPick($event)" :disabled="uploading" />
        <span v-if="uploading">uploading…</span>
        <span v-if="error" class="err">{{ error.message }}</span>
      </div>
    </div>
  `,
  setup(props, { emit }) {
    const composable = ref(null);
    if (props.agentId) composable.value = useAgentFiles(props.agentId);
    const uploading = ref(false);
    const error = ref(null);

    async function onPick(e) {
      if (!props.agentId) return;
      const file = e.target.files[0];
      if (!file) return;
      uploading.value = true;
      try {
        if (!composable.value) composable.value = useAgentFiles(props.agentId);
        const result = await composable.value.upload(file);
        emit('change', [...props.files, result]);
      } catch (err) { error.value = err; }
      finally { uploading.value = false; e.target.value = ''; }
    }
    async function onDetach(fileId) {
      if (!composable.value) composable.value = useAgentFiles(props.agentId);
      try {
        await composable.value.detach(fileId);
        emit('change', props.files.filter(f => f.id !== fileId));
      } catch (err) { error.value = err; }
    }
    return { uploading, error, onPick, onDetach };
  },
});
```

- [ ] **Step 5: `trigger-list.js`**

```js
import { defineComponent, onMounted, watch } from 'vue';
import { useTriggers } from '/composables/useTriggers.js';

export default defineComponent({
  name: 'TriggerList',
  props: { agentId: { type: String, default: null } },
  template: `
    <div class="trigger-list">
      <div v-if="!agentId" class="picker__hint">Save the agent first to see triggers.</div>
      <div v-else-if="loading">loading…</div>
      <div v-else-if="webhooks.length === 0 && schedules.length === 0" class="picker__hint">
        No triggers configured. Add via <code>[[webhooks]]</code> / <code>[[schedules]]</code> in <code>simulacra.toml</code>.
      </div>
      <div v-else>
        <div v-for="w in webhooks" :key="w.path" class="trigger-row">
          <span class="dim">webhook</span> {{ w.path }}
          <span v-if="w.hmac" class="badge">HMAC</span>
        </div>
        <div v-for="s in schedules" :key="s.cron" class="trigger-row">
          <span class="dim">cron</span> {{ s.cron }} <span class="dim">({{ s.missed_policy }})</span>
        </div>
      </div>
    </div>
  `,
  setup(props) {
    const { webhooks, schedules, loading, list } = useTriggers();
    function refresh() { if (props.agentId) list(props.agentId); }
    onMounted(refresh);
    watch(() => props.agentId, refresh);
    return { webhooks, schedules, loading };
  },
});
```

- [ ] **Step 6: Add picker CSS to `styles.css`**

Append:

```css
.picker { display: flex; flex-direction: column; gap: var(--space-2); }
.picker__chips { display: flex; flex-wrap: wrap; gap: var(--space-1); }
.picker__hint { font-size: 12px; color: var(--muted); }
.picker__inline-create { display: flex; gap: var(--space-1); margin-top: var(--space-2); }
.chip { display: inline-flex; align-items: center; gap: 4px; }
.chip__x { background: transparent; border: 0; cursor: pointer; padding: 0 2px; }
.tool-picker__row { display: flex; align-items: center; gap: var(--space-1); font-size: 13px; padding: 2px 0; }
.file-row { display: flex; justify-content: space-between; align-items: center; padding: 2px 0; }
.dim { color: var(--muted); font-size: 11px; }
.err { color: var(--danger); font-size: 12px; }
.trigger-row { padding: 4px 0; font-size: 12px; }
.badge { padding: 1px 6px; background: var(--hover); border-radius: 8px; font-size: 10px; }
```

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-frontend/assets/components/pickers crates/simulacra-frontend/assets/styles.css
git commit -m "feat(simulacra-frontend): pickers (channel, tool, skill, file, trigger) [S048]"
```

---

### Task 19: Extend `useAgents` with create / update / saveAndRun

**Files:**
- Modify: `crates/simulacra-frontend/assets/composables/useAgents.js`
- Modify: `crates/simulacra-frontend/assets/composables/useAgents.test.mjs`

- [ ] **Step 1: Add failing tests** to the existing `useAgents.test.mjs`

```js
test('create(input) issues createAgent mutation and returns the new agent', async () => {
  let calls = 0;
  globalThis.fetch = async (url, opts) => {
    calls++;
    const body = JSON.parse(opts.body);
    if (calls === 1) {
      assert.match(body.query, /createAgent/);
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { createAgent: { id: 'new1', name: 'foo' } } }) };
    }
    return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { agents: { edges: [] } } }) };
  };
  const { create } = useAgents();
  const agent = await create({ name: 'foo', systemPrompt: 'be helpful', capabilities: [], skillIds: [], channelIds: [] });
  assert.equal(agent.id, 'new1');
});

test('update(id, patch) issues updateAgent mutation', async () => {
  globalThis.fetch = async (url, opts) => {
    const body = JSON.parse(opts.body);
    assert.match(body.query, /updateAgent/);
    assert.equal(body.variables.id, 'a1');
    return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { updateAgent: { id: 'a1', name: 'patched' } } }) };
  };
  const { update } = useAgents();
  const out = await update('a1', { name: 'patched' });
  assert.equal(out.name, 'patched');
});

test('saveAndRun(input, taskPrompt) creates/updates then POSTs to /tasks/create', async () => {
  let phase = 0;
  globalThis.fetch = async (url, opts) => {
    phase++;
    if (phase === 1) {
      // mutation
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { createAgent: { id: 'a2', name: 'foo' } } }) };
    }
    if (phase === 2) {
      // list refresh after mutation
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { agents: { edges: [] } } }) };
    }
    // POST /api/v1/tasks/create
    assert.equal(url, '/api/v1/tasks/create');
    const body = JSON.parse(opts.body);
    assert.equal(body.task, 'do the thing');
    assert.equal(body.agent_type, 'foo');
    return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ task_id: 'task_xyz' }) };
  };
  const { saveAndRun } = useAgents();
  const taskId = await saveAndRun({ name: 'foo', systemPrompt: 'p', capabilities: [], skillIds: [], channelIds: [] }, 'do the thing');
  assert.equal(taskId, 'task_xyz');
});
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useAgents.test.mjs
```

- [ ] **Step 3: Extend `useAgents.js`**

Replace the existing `useAgents.js` with:

```js
import { ref } from 'vue';
import { gql } from '/api/graphql.js';
import { restJson } from '/api/rest.js';

const LIST_QUERY = `
  query {
    agents(page: { first: 100 }) {
      edges {
        node {
          id name updatedAt
          channels { id name kind }
          tools { id name kind }
          skills { id name }
          files { id name size }
        }
      }
    }
  }
`;

const GET_QUERY = `
  query($id: ID!) {
    agent(id: $id) {
      id name systemPrompt updatedAt
      capabilities
      channels { id name kind config }
      tools { id name kind capabilities }
      skills { id name }
      files { id name size }
    }
  }
`;

const CREATE = `
  mutation($input: CreateAgentInput!) {
    createAgent(input: $input) { id name }
  }
`;

const UPDATE = `
  mutation($id: ID!, $input: UpdateAgentInput!) {
    updateAgent(id: $id, input: $input) { id name }
  }
`;

export function useAgents() {
  const agents = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function list() {
    loading.value = true; error.value = null;
    try {
      const data = await gql(LIST_QUERY);
      agents.value = data.agents.edges.map(e => e.node);
    } catch (e) { error.value = e; agents.value = []; }
    finally { loading.value = false; }
  }

  async function get(id) {
    loading.value = true; error.value = null;
    try { return (await gql(GET_QUERY, { id })).agent; }
    catch (e) { error.value = e; return null; }
    finally { loading.value = false; }
  }

  async function create(input) {
    error.value = null;
    try {
      const data = await gql(CREATE, { input });
      await list();
      return data.createAgent;
    } catch (e) { error.value = e; throw e; }
  }

  async function update(id, input) {
    error.value = null;
    try {
      const data = await gql(UPDATE, { id, input });
      await list();
      return data.updateAgent;
    } catch (e) { error.value = e; throw e; }
  }

  async function saveAndRun(input, taskPrompt, existingId) {
    const saved = existingId
      ? await update(existingId, input)
      : await create(input);
    const result = await restJson('/api/v1/tasks/create', {
      method: 'POST',
      body: { task: taskPrompt, agent_type: saved.name },
    });
    return result.task_id;
  }

  return { agents, loading, error, list, get, create, update, saveAndRun };
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useAgents.test.mjs
```

Expected: original 3 tests + 3 new tests all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useAgents.js crates/simulacra-frontend/assets/composables/useAgents.test.mjs
git commit -m "feat(simulacra-frontend): useAgents create/update/saveAndRun [S048]"
```

---

### Task 20: `agent-form` component (two-column layout, Save / Save & Run)

**Files:**
- Create: `crates/simulacra-frontend/assets/components/agent-form.js`

- [ ] **Step 1: Implement the form**

```js
import { defineComponent, ref, onMounted, watch, computed } from 'vue';
import { useAgents } from '/composables/useAgents.js';
import { showToast } from '/components/app-shell.js';
import ChannelPicker from '/components/pickers/channel-picker.js';
import ToolPicker from '/components/pickers/tool-picker.js';
import SkillPicker from '/components/pickers/skill-picker.js';
import FileUploader from '/components/pickers/file-uploader.js';
import TriggerList from '/components/pickers/trigger-list.js';

export default defineComponent({
  name: 'AgentForm',
  props: { id: { type: String, default: null } },
  components: { ChannelPicker, ToolPicker, SkillPicker, FileUploader, TriggerList },
  template: `
    <form class="agent-form" @submit.prevent="onSave">
      <div class="agent-form__crumbs">
        <router-link to="/">Agents</router-link>
        <span class="dim">/</span>
        <span>{{ isEdit ? 'Edit' : 'New' }}</span>
      </div>

      <div class="agent-form__grid">
        <div class="agent-form__meta">
          <div class="field">
            <div class="label">Title</div>
            <input v-model="form.name" required placeholder="my-agent" />
          </div>

          <div class="field">
            <div class="label">Channels</div>
            <channel-picker v-model="form.channelIds" />
          </div>

          <div class="field">
            <div class="label">Tools</div>
            <tool-picker v-model="form.capabilities" />
          </div>

          <div class="field">
            <div class="label">Skill</div>
            <skill-picker v-model="form.skillId" />
          </div>

          <div class="field">
            <div class="label">Triggers (read-only)</div>
            <trigger-list :agent-id="id" />
          </div>
        </div>

        <div class="agent-form__main">
          <div class="field">
            <div class="label">Instructions</div>
            <textarea v-model="form.systemPrompt" rows="14" placeholder="You are a..."></textarea>
          </div>
          <div class="field">
            <div class="label">Files</div>
            <file-uploader :agent-id="id" :files="form.files" @change="form.files = $event" />
          </div>

          <div class="agent-form__actions">
            <router-link to="/"><button type="button">Cancel</button></router-link>
            <button type="submit" :disabled="saving">{{ saving ? 'Saving…' : 'Save' }}</button>
            <button type="button" class="primary" @click="onSaveAndRun" :disabled="saving">▶ Save &amp; Run</button>
          </div>

          <div v-if="runDialog" class="run-dialog">
            <div class="run-dialog__panel">
              <div class="label">What should this agent do?</div>
              <textarea v-model="runPrompt" rows="4" placeholder="describe the task…"></textarea>
              <div class="agent-form__actions">
                <button type="button" @click="runDialog = false">cancel</button>
                <button type="button" class="primary" @click="confirmRun">Run</button>
              </div>
            </div>
          </div>
        </div>
      </div>
    </form>
  `,
  setup(props) {
    const { get, create, update, saveAndRun } = useAgents();
    const form = ref({
      name: '',
      systemPrompt: '',
      capabilities: [],
      channelIds: [],
      skillId: null,
      files: [],
    });
    const saving = ref(false);
    const runDialog = ref(false);
    const runPrompt = ref('');

    const isEdit = computed(() => !!props.id);

    async function load() {
      if (!props.id) return;
      const a = await get(props.id);
      if (!a) { showToast('Agent not found'); return; }
      form.value = {
        name: a.name,
        systemPrompt: a.systemPrompt,
        capabilities: a.capabilities || [],
        channelIds: (a.channels || []).map(c => c.id),
        skillId: (a.skills && a.skills[0]?.id) || null,
        files: a.files || [],
      };
    }

    onMounted(load);
    watch(() => props.id, load);

    function buildInput() {
      return {
        name: form.value.name,
        systemPrompt: form.value.systemPrompt,
        capabilities: form.value.capabilities,
        channelIds: form.value.channelIds,
        skillIds: form.value.skillId ? [form.value.skillId] : [],
      };
    }

    async function onSave() {
      saving.value = true;
      try {
        if (isEdit.value) await update(props.id, buildInput());
        else await create(buildInput());
        window.location.hash = '#/';
      } catch (e) {
        showToast(`Save failed: ${e.message}`);
      } finally { saving.value = false; }
    }

    function onSaveAndRun() {
      runDialog.value = true;
      runPrompt.value = '';
    }

    async function confirmRun() {
      saving.value = true;
      runDialog.value = false;
      try {
        const taskId = await saveAndRun(buildInput(), runPrompt.value, props.id || undefined);
        // Navigate to the run route. We need the saved agent id.
        // For new agents, we don't have the id yet — saveAndRun returns taskId only.
        // Refetch the just-created agent by name to get its id.
        // Simpler: use the saved.name and look it up. But the run route
        // is keyed by agent id, so we need it. Best path: have saveAndRun
        // return both. v1 workaround: navigate to run with name as a placeholder
        // — agent-run resolves by agentId, so we need the real id.
        // We'll iterate: Task 21 wires saveAndRun to return { taskId, agentId }.
        const targetId = props.id || form.value.name; // resolved properly in Task 21
        window.location.hash = `#/agents/${targetId}/run/${taskId}`;
      } catch (e) {
        showToast(`Run failed: ${e.message}`);
      } finally { saving.value = false; }
    }

    return { form, saving, runDialog, runPrompt, isEdit, onSave, onSaveAndRun, confirmRun };
  },
});
```

- [ ] **Step 2: Add form CSS to `styles.css`**

Append:

```css
.agent-form { max-width: 1100px; margin: 0 auto; }
.agent-form__crumbs { font-size: 12px; color: var(--muted); margin-bottom: var(--space-3); }
.agent-form__crumbs a { color: inherit; }
.agent-form__grid { display: grid; grid-template-columns: 320px 1fr; gap: var(--space-6); }
.agent-form__meta, .agent-form__main { display: flex; flex-direction: column; gap: var(--space-3); }
.field { display: flex; flex-direction: column; gap: var(--space-1); }
.field input, .field select, .field textarea { width: 100%; }
.agent-form__actions { display: flex; gap: var(--space-2); justify-content: flex-end; margin-top: var(--space-3); }
.run-dialog { position: fixed; inset: 0; background: rgba(0,0,0,0.3); display: flex; align-items: center; justify-content: center; z-index: 30; }
.run-dialog__panel { background: var(--bg); padding: var(--space-4); border-radius: var(--radius); width: 480px; box-shadow: 0 4px 20px rgba(0,0,0,0.15); }
.run-dialog__panel textarea { width: 100%; }
```

- [ ] **Step 3: Manual smoke**

Run dev_server. Browse to:
- `#/agents/new` — empty form. Type name, instructions, click Save → navigates to `/`. Verify the new agent appears.
- `#/agents/:id` (use the seeded agent's id) — pre-populated form. Modify, click Save → navigates back. Verify.
- `#/agents/new` → fill form → Save & Run → enter task prompt → confirm → navigates to run route (which still 404s its component until Phase E).

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-frontend/assets/components/agent-form.js crates/simulacra-frontend/assets/styles.css
git commit -m "feat(simulacra-frontend): agent-form with two-column layout + Save & Run [S048]"
```

---

### Task 21: Wire saveAndRun to return both taskId and agentId

**Files:**
- Modify: `crates/simulacra-frontend/assets/composables/useAgents.js`
- Modify: `crates/simulacra-frontend/assets/composables/useAgents.test.mjs`
- Modify: `crates/simulacra-frontend/assets/components/agent-form.js`

The form's `confirmRun` left a comment that `saveAndRun` should return both ids. Doing it as its own task keeps the diff focused.

- [ ] **Step 1: Update the test**

Replace the `saveAndRun` test in `useAgents.test.mjs`:

```js
test('saveAndRun returns { taskId, agentId, agentName }', async () => {
  let phase = 0;
  globalThis.fetch = async (url, opts) => {
    phase++;
    if (phase === 1) {
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { createAgent: { id: 'a2', name: 'foo' } } }) };
    }
    if (phase === 2) {
      return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ data: { agents: { edges: [] } } }) };
    }
    return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => ({ task_id: 'task_xyz' }) };
  };
  const { saveAndRun } = useAgents();
  const result = await saveAndRun({ name: 'foo', systemPrompt: 'p', capabilities: [], skillIds: [], channelIds: [] }, 'do the thing');
  assert.equal(result.taskId, 'task_xyz');
  assert.equal(result.agentId, 'a2');
  assert.equal(result.agentName, 'foo');
});
```

- [ ] **Step 2: Run test to verify it fails**

- [ ] **Step 3: Update `saveAndRun` in `useAgents.js`**

```js
async function saveAndRun(input, taskPrompt, existingId) {
  const saved = existingId
    ? await update(existingId, input)
    : await create(input);
  const result = await restJson('/api/v1/tasks/create', {
    method: 'POST',
    body: { task: taskPrompt, agent_type: saved.name },
  });
  return { taskId: result.task_id, agentId: saved.id, agentName: saved.name };
}
```

- [ ] **Step 4: Update `agent-form.js` confirmRun**

Replace the `confirmRun` body:

```js
async function confirmRun() {
  saving.value = true;
  runDialog.value = false;
  try {
    const { taskId, agentId } = await saveAndRun(buildInput(), runPrompt.value, props.id || undefined);
    window.location.hash = `#/agents/${agentId}/run/${taskId}`;
  } catch (e) {
    showToast(`Run failed: ${e.message}`);
  } finally { saving.value = false; }
}
```

- [ ] **Step 5: Run test to verify it passes**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useAgents.test.mjs
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useAgents.js crates/simulacra-frontend/assets/composables/useAgents.test.mjs crates/simulacra-frontend/assets/components/agent-form.js
git commit -m "feat(simulacra-frontend): saveAndRun returns agentId for run navigation [S048]"
```

---

**Phase D acceptance:** Tasks 14–21 complete. Use cases 1, 3, and 5 (read-only) work end-to-end. Use case 4 (run) is unblocked at the navigation level — Phase E renders the run view.

---

## Phase E — Run view

### Task 22: `useTaskStream` composable (SSE → reactive events[])

**Files:**
- Create: `crates/simulacra-frontend/assets/composables/useTaskStream.js`
- Create: `crates/simulacra-frontend/assets/composables/useTaskStream.test.mjs`

- [ ] **Step 1: Write the failing test**

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';

class MockEventSource {
  constructor(url) {
    this.url = url;
    this.listeners = {};
    MockEventSource.lastInstance = this;
  }
  addEventListener(type, fn) { this.listeners[type] = fn; }
  close() { this.closed = true; }
  emit(type, data) { this.listeners[type]?.({ data: typeof data === 'string' ? data : JSON.stringify(data) }); }
}
globalThis.EventSource = MockEventSource;

const { useTaskStream } = await import('./useTaskStream.js');

test('opens EventSource and pushes events into reactive array', () => {
  const { events, status, open, close } = useTaskStream();
  open('task_x');
  assert.equal(MockEventSource.lastInstance.url, '/api/v1/tasks/task_x/events');
  MockEventSource.lastInstance.emit('message', { type: 'token', text: 'hi' });
  MockEventSource.lastInstance.emit('message', { type: 'task_complete' });
  assert.equal(events.value.length, 2);
  assert.equal(status.value, 'completed');
  close();
});

test('close() shuts down EventSource', () => {
  const { open, close } = useTaskStream();
  open('task_x');
  close();
  assert.equal(MockEventSource.lastInstance.closed, true);
});

test('error event sets status to error', () => {
  const { open, status } = useTaskStream();
  open('task_x');
  MockEventSource.lastInstance.listeners.error?.({});
  // reconnect attempt happens — status remains "running" until second error
  MockEventSource.lastInstance.listeners.error?.({});
  assert.equal(status.value, 'error');
});
```

- [ ] **Step 2: Run test to verify it fails**

- [ ] **Step 3: Implement `useTaskStream.js`**

```js
import { ref } from 'vue';

const TERMINAL_TYPES = new Set(['task_complete', 'task_failed', 'task_cancelled']);

export function useTaskStream() {
  const events = ref([]);
  const status = ref('idle'); // idle | running | completed | failed | error
  const error = ref(null);
  let source = null;
  let attemptedReconnect = false;
  let currentTaskId = null;

  function open(taskId) {
    currentTaskId = taskId;
    status.value = 'running';
    error.value = null;
    events.value = [];
    attemptedReconnect = false;
    connect();
  }

  function connect() {
    source = new EventSource(`/api/v1/tasks/${currentTaskId}/events`);
    source.addEventListener('message', (e) => {
      try {
        const ev = JSON.parse(e.data);
        events.value = [...events.value, ev];
        if (TERMINAL_TYPES.has(ev.type)) {
          status.value = ev.type === 'task_complete' ? 'completed' : 'failed';
          source?.close();
        }
      } catch (parseErr) {
        error.value = parseErr;
      }
    });
    source.addEventListener('error', () => {
      if (!attemptedReconnect && status.value === 'running') {
        attemptedReconnect = true;
        source?.close();
        connect();
      } else {
        status.value = 'error';
        error.value = new Error('stream interrupted — task may still be running');
        source?.close();
      }
    });
  }

  function close() {
    source?.close();
    source = null;
  }

  return { events, status, error, open, close };
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd crates/simulacra-frontend/assets/composables && node --test useTaskStream.test.mjs
```

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useTaskStream.js crates/simulacra-frontend/assets/composables/useTaskStream.test.mjs
git commit -m "feat(simulacra-frontend): useTaskStream composable with SSE + reconnect [S048]"
```

---

### Task 23: `useTaskArtifacts` composable

**Files:**
- Create: `crates/simulacra-frontend/assets/composables/useTaskArtifacts.js`
- Create: `crates/simulacra-frontend/assets/composables/useTaskArtifacts.test.mjs`

- [ ] **Step 1: Write the failing test**

```js
import { test } from 'node:test';
import assert from 'node:assert/strict';

let mockResponse, lastUrl;
globalThis.fetch = async (url) => {
  lastUrl = url;
  return { ok: true, status: 200, headers:{get:()=>'application/json'}, json: async () => mockResponse };
};

const { useTaskArtifacts } = await import('./useTaskArtifacts.js');

test('refresh(taskId) lists artifacts at /api/v1/tasks/:id/artifacts', async () => {
  mockResponse = { artifacts: [{ path: 'duplicates.csv', size: 1024 }] };
  const { artifacts, refresh } = useTaskArtifacts();
  await refresh('task_x');
  assert.equal(lastUrl, '/api/v1/tasks/task_x/artifacts');
  assert.equal(artifacts.value.length, 1);
  assert.equal(artifacts.value[0].path, 'duplicates.csv');
});

test('downloadUrl(taskId, path) returns the artifact byte URL', () => {
  const { downloadUrl } = useTaskArtifacts();
  assert.equal(downloadUrl('task_x', 'sub/file.csv'), '/api/v1/tasks/task_x/artifacts/sub/file.csv');
});
```

- [ ] **Step 2: Run test to verify it fails**

- [ ] **Step 3: Implement `useTaskArtifacts.js`**

```js
import { ref } from 'vue';
import { restJson } from '/api/rest.js';

export function useTaskArtifacts() {
  const artifacts = ref([]);
  const loading = ref(false);
  const error = ref(null);

  async function refresh(taskId) {
    loading.value = true; error.value = null;
    try {
      const data = await restJson(`/api/v1/tasks/${taskId}/artifacts`);
      artifacts.value = data.artifacts || [];
    } catch (e) { error.value = e; artifacts.value = []; }
    finally { loading.value = false; }
  }

  function downloadUrl(taskId, path) {
    return `/api/v1/tasks/${taskId}/artifacts/${path}`;
  }

  return { artifacts, loading, error, refresh, downloadUrl };
}
```

- [ ] **Step 4: Run test to verify it passes**

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-frontend/assets/composables/useTaskArtifacts.js crates/simulacra-frontend/assets/composables/useTaskArtifacts.test.mjs
git commit -m "feat(simulacra-frontend): useTaskArtifacts composable [S048]"
```

---

### Task 24: Activity event renderers

**Files:**
- Create: `crates/simulacra-frontend/assets/components/activity/event-token.js`
- Create: `crates/simulacra-frontend/assets/components/activity/event-thinking.js`
- Create: `crates/simulacra-frontend/assets/components/activity/event-tool-call.js`
- Create: `crates/simulacra-frontend/assets/components/activity/event-child.js`
- Create: `crates/simulacra-frontend/assets/components/activity/artifact-sidebar.js`

These five renderers are presentational. No unit tests in v1 — they are exercised by the run view's manual smoke.

- [ ] **Step 1: `event-token.js` — streamed model output**

```js
import { defineComponent } from 'vue';

export default defineComponent({
  name: 'EventToken',
  props: { event: { type: Object, required: true } },
  template: `<span class="ev-token">{{ event.text }}</span>`,
});
```

- [ ] **Step 2: `event-thinking.js` — collapsible thinking block**

```js
import { defineComponent, ref } from 'vue';

export default defineComponent({
  name: 'EventThinking',
  props: { event: { type: Object, required: true } },
  template: `
    <div class="ev-thinking" :class="{ 'ev-thinking--open': open }">
      <button class="ev-thinking__toggle" @click="open = !open">
        {{ open ? '▾' : '▸' }} thinking
      </button>
      <pre v-if="open" class="ev-thinking__body">{{ event.text }}</pre>
    </div>
  `,
  setup() {
    const open = ref(false);
    return { open };
  },
});
```

- [ ] **Step 3: `event-tool-call.js` — tool invocation**

```js
import { defineComponent, ref } from 'vue';

export default defineComponent({
  name: 'EventToolCall',
  props: { event: { type: Object, required: true } },
  template: `
    <div class="ev-tool" :class="{ 'ev-tool--open': open, 'ev-tool--error': event.error }">
      <button class="ev-tool__head" @click="open = !open">
        {{ open ? '▾' : '▸' }} <strong>{{ event.tool }}</strong>
        <span v-if="event.summary" class="dim">{{ event.summary }}</span>
        <span v-if="event.duration_ms" class="dim">· {{ event.duration_ms }}ms</span>
      </button>
      <div v-if="open" class="ev-tool__body">
        <div v-if="event.args" class="ev-tool__section">
          <div class="label">args</div>
          <pre>{{ JSON.stringify(event.args, null, 2) }}</pre>
        </div>
        <div v-if="event.result !== undefined" class="ev-tool__section">
          <div class="label">result</div>
          <pre>{{ typeof event.result === 'string' ? event.result : JSON.stringify(event.result, null, 2) }}</pre>
        </div>
        <div v-if="event.error" class="ev-tool__section ev-tool__error">
          <div class="label">error</div>
          <pre>{{ event.error }}</pre>
        </div>
      </div>
    </div>
  `,
  setup() {
    const open = ref(false);
    return { open };
  },
});
```

- [ ] **Step 4: `event-child.js` — sub-agent event**

```js
import { defineComponent, ref } from 'vue';

export default defineComponent({
  name: 'EventChild',
  props: { event: { type: Object, required: true } },
  template: `
    <div class="ev-child" :class="{ 'ev-child--open': open }">
      <button class="ev-child__head" @click="open = !open">
        {{ open ? '▾' : '▸' }} sub-agent: {{ event.agent_type }}
        <span v-if="event.task_id" class="dim">{{ event.task_id }}</span>
      </button>
      <div v-if="open" class="ev-child__body">
        <pre>{{ JSON.stringify(event.events || [], null, 2) }}</pre>
      </div>
    </div>
  `,
  setup() {
    const open = ref(false);
    return { open };
  },
});
```

- [ ] **Step 5: `artifact-sidebar.js`**

```js
import { defineComponent } from 'vue';

export default defineComponent({
  name: 'ArtifactSidebar',
  props: {
    artifacts: { type: Array, required: true },
    taskId: { type: String, required: true },
  },
  template: `
    <aside class="artifacts">
      <div class="label">Artifacts</div>
      <div v-if="artifacts.length === 0" class="dim">none yet</div>
      <a
        v-for="a in artifacts"
        :key="a.path"
        class="artifacts__row"
        :href="downloadHref(a.path)"
        target="_blank"
        download
      >
        <span>📄 {{ a.path }}</span>
        <span class="dim">{{ formatSize(a.size) }} · ⬇</span>
      </a>
    </aside>
  `,
  methods: {
    downloadHref(path) { return `/api/v1/tasks/${this.taskId}/artifacts/${path}`; },
    formatSize(b) {
      if (b == null) return '';
      if (b < 1024) return `${b}B`;
      if (b < 1024 * 1024) return `${(b/1024).toFixed(1)}KB`;
      return `${(b/1024/1024).toFixed(1)}MB`;
    },
  },
});
```

- [ ] **Step 6: Add activity CSS to `styles.css`**

Append:

```css
.ev-token { font-family: ui-monospace, monospace; white-space: pre-wrap; }
.ev-thinking, .ev-tool, .ev-child { margin: 4px 0; }
.ev-thinking__toggle, .ev-tool__head, .ev-child__head { background: transparent; border: 0; cursor: pointer; padding: 0; font: inherit; text-align: left; }
.ev-thinking__body, .ev-tool__body, .ev-child__body { margin-left: 16px; padding: var(--space-2); background: var(--hover); border-radius: 4px; font-family: ui-monospace, monospace; font-size: 11px; white-space: pre-wrap; }
.ev-tool__section { margin-top: var(--space-1); }
.ev-tool__error { color: var(--danger); }

.artifacts { display: flex; flex-direction: column; gap: var(--space-1); }
.artifacts__row { display: flex; justify-content: space-between; padding: 6px 0; border-bottom: 1px solid var(--border); font-size: 12px; color: inherit; text-decoration: none; }
.artifacts__row:hover { background: var(--hover); }
```

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-frontend/assets/components/activity crates/simulacra-frontend/assets/styles.css
git commit -m "feat(simulacra-frontend): activity event renderers + artifact sidebar [S048]"
```

---

### Task 25: `agent-run` component — the run page itself

**Files:**
- Create: `crates/simulacra-frontend/assets/components/agent-run.js`

- [ ] **Step 1: Implement the component**

```js
import { defineComponent, onMounted, onUnmounted, watch } from 'vue';
import { useTaskStream } from '/composables/useTaskStream.js';
import { useTaskArtifacts } from '/composables/useTaskArtifacts.js';
import EventToken from '/components/activity/event-token.js';
import EventThinking from '/components/activity/event-thinking.js';
import EventToolCall from '/components/activity/event-tool-call.js';
import EventChild from '/components/activity/event-child.js';
import ArtifactSidebar from '/components/activity/artifact-sidebar.js';

const RENDERERS = {
  token: EventToken,
  think_start: null,
  think_delta: EventThinking,
  tool_start: EventToolCall,
  tool_end: EventToolCall,
  child_activity: EventChild,
};

export default defineComponent({
  name: 'AgentRun',
  props: {
    id: { type: String, required: true },
    taskId: { type: String, required: true },
  },
  components: { EventToken, EventThinking, EventToolCall, EventChild, ArtifactSidebar },
  template: `
    <div class="agent-run">
      <div class="agent-run__crumbs">
        <router-link to="/">Agents</router-link>
        <span class="dim">/</span>
        <router-link :to="'/agents/' + id">{{ id }}</router-link>
        <span class="dim">/ run · {{ taskId }} · {{ status }}</span>
      </div>
      <div class="agent-run__grid">
        <section class="agent-run__feed">
          <div v-if="events.length === 0 && status === 'running'" class="dim">connecting…</div>
          <component
            v-for="(ev, i) in events"
            :key="i"
            :is="rendererFor(ev.type)"
            :event="ev"
            v-if="rendererFor(ev.type)"
          />
          <div v-if="error" class="err">{{ error.message }}</div>
        </section>
        <artifact-sidebar :artifacts="artifacts" :task-id="taskId" />
      </div>
    </div>
  `,
  setup(props) {
    const { events, status, error, open, close } = useTaskStream();
    const { artifacts, refresh: refreshArtifacts } = useTaskArtifacts();

    onMounted(() => {
      open(props.taskId);
      refreshArtifacts(props.taskId);
    });
    onUnmounted(() => close());

    // Refresh artifacts on each event of type 'artifact_written' (best effort).
    watch(events, (next, prev) => {
      const prevLen = prev?.length ?? 0;
      for (let i = prevLen; i < next.length; i++) {
        if (next[i].type === 'artifact_written') {
          refreshArtifacts(props.taskId);
          break;
        }
      }
    });

    function rendererFor(type) { return RENDERERS[type] ?? null; }

    return { events, status, error, artifacts, rendererFor };
  },
});
```

- [ ] **Step 2: Add run-page CSS to `styles.css`**

Append:

```css
.agent-run { max-width: 1400px; margin: 0 auto; }
.agent-run__crumbs { font-size: 12px; color: var(--muted); margin-bottom: var(--space-3); }
.agent-run__crumbs a { color: inherit; }
.agent-run__grid { display: grid; grid-template-columns: 1fr 280px; gap: var(--space-4); }
.agent-run__feed { border: 1px solid var(--border); border-radius: var(--radius); padding: var(--space-3); min-height: 60vh; font-family: ui-monospace, monospace; font-size: 12px; line-height: 1.5; }
```

- [ ] **Step 3: Manual smoke**

Run dev_server. Build a fresh agent via `/agents/new`, click Save & Run, enter a task prompt, confirm. Expect:
- Navigates to `#/agents/:id/run/:taskId`
- "connecting…" appears, then events stream in
- Tool calls render with collapsible bodies
- Artifact sidebar populates as the agent writes files
- Status transitions to `completed` on terminal event

Note: full end-to-end requires a real provider. With S043's stub provider you can simulate one; otherwise the SSE stream may emit tool events but not real LLM-driven activity. The visual rendering is what matters — even a synthetic event stream proves the renderer works.

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-frontend/assets/components/agent-run.js crates/simulacra-frontend/assets/styles.css
git commit -m "feat(simulacra-frontend): agent-run component (activity feed + artifact sidebar) [S048]"
```

---

**Phase E acceptance:** Tasks 22–25 complete. Use case 4 works end-to-end: select agent → Run → activity feed streams in real time → artifacts download.

---

## Phase F — Closure

### Task 26: Update `SPECS.md` to mark S048 Active

**Files:**
- Modify: `SPECS.md`

- [ ] **Step 1: Update the entry**

Change `S048` row's Status column from `Draft` to `Active`. The description column can stay or get a small tweak noting that v1 is fully landed.

- [ ] **Step 2: Commit**

```bash
git add SPECS.md
git commit -m "spec(SPECS): mark S048 Active after simulacra-frontend v1 lands [S048]"
```

---

### Task 27: Manual smoke walkthrough — end-to-end v1 acceptance

This is not a TDD task; it is the v1 manual-smoke gate from the spec.

- [ ] **Step 1: Boot the dev server**

```bash
cargo run -p simulacra-server --example dev_server
```

- [ ] **Step 2: Walk through every smoke item**

Open http://127.0.0.1:8080 and verify each line:

- [ ] `#/` renders the agent list with a card grid and a "+ New agent" button
- [ ] Clicking a card opens a drawer showing the agent's full composition with Edit and Run buttons
- [ ] `#/agents/new` renders the form with empty fields and Save / Save & Run buttons
- [ ] `#/agents/:id` renders the form with fields populated from the agent, including the read-only triggers section in the meta column
- [ ] Save returns to the list view; the new/edited agent is visible
- [ ] Save & Run opens the task-prompt dialog; submitting it navigates to the run route
- [ ] `#/agents/:id/run/:taskId` renders the activity feed; events stream in real time; artifacts populate as the task writes them; status transitions to `completed` on terminal event
- [ ] Toast appears for any failed save / run

- [ ] **Step 3: If any smoke item fails**

Open a follow-up task with the failure noted. The plan's contract is that v1 ships when every line above is true; anything missing is a v1 bug, not a v2 feature.

- [ ] **Step 4: Final mechanical check**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

All four must pass.

- [ ] **Step 5: Phase 4 review (per CLAUDE.md)**

- 4a — Run `copilot --model gpt-5.4` review on the full diff with focus on spec compliance, capability enforcement, and edge cases
- 4b — Claude sub-agent holistic review

Address every BLOCKER. Commit any fixes as additional commits in the S048 chain.

---

## Self-review

Before handing off, the plan author runs this checklist:

**1. Spec coverage** — every assertion in `specs/S048-simulacra-frontend.md` maps to a task here, with three honest gaps called out:

| Spec assertion group | Task(s) | Notes |
|---|---|---|
| Crate scaffolding × 5 | Task 1 | The `GET /styles.css` 200/`text/css` assertion is structurally covered by ServeDir's behavior; the `frontend_mount` test exercises the same code path via `/index.html`. An explicit `/styles.css` test can be added once Task 9 creates the file (low priority — same code path). |
| Mount glue × 4 | Tasks 4, 6 | The two toggle-related assertions ("when `[server.frontend] enabled = true`" / "Disabling `[server.frontend]` removes the static routes") are NOT covered in v1 because simulacra-server has no `[server.frontend]` config section yet. v1 always mounts. The toggle naturally lands with the production `simulacra serve` CLI in a follow-up spec. Mark these two assertions as `- [ ]` (open) in S048 after Phase F; do not check them. |
| NoAuthProvider × 3 | Tasks 2, 3 | "When `dev_mode = false` and no other provider is set, simulacra-server startup fails" is similarly bound to the production CLI's config-resolution path and is open in v1. |
| Triggers endpoint × 5 | Task 6 | All five assertions covered by the four tests in `tests/triggers_endpoint.rs`. |
| Composables × 8 | Tasks 10–12, 14–17, 19, 22, 23 | All eight composables have unit tests with mocked `fetch` / `EventSource`. |
| Routes (manual smoke) | Task 27 | Prose in spec, not `- [ ]` checkboxes; v1 acceptance is hand-validated. |

The three open-in-v1 assertions are a deliberate scope cut: they require the production CLI's config-driven provider/mount selection, which is its own follow-up. They stay listed as unchecked in the spec — that's the spec lifecycle pattern (specs are living documents; assertions become checked as work lands).

**2. Placeholder scan** — scanned for "TBD", "TODO", "fill in", "similar to". One implementer-note in Task 4 references the canonical `graphql_router(...)` signature in code rather than reproducing it — this is acceptable because it is a pointer, not a placeholder. The constructor-default note in Task 6's test code is also a pointer (acknowledging the WebhookConfig/ScheduleConfig field set), not a TODO.

**3. Type consistency** — `gql`, `restJson`, `restMultipart`, `openTaskStream`, `useAgents`, `useChannels`, `useTools`, `useSkills`, `useAgentFiles`, `useTriggers`, `useTaskStream`, `useTaskArtifacts`, `NoAuthProvider`, `NoAuthGraphQLProvider`, `frontend_router`, `AppState::with_triggers`, `list_triggers`, `TriggersQuery` — all consistently named across tasks.

**4. Scope check** — single coherent feature with phases that allow incremental landing. Each phase's acceptance is testable on its own.

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-05-s048-simulacra-frontend-plan.md`. Two execution options:

**1. Subagent-Driven (recommended)** — dispatch a fresh subagent per task per CLAUDE.md's protocol (Phase 1 GPT-5.4 red, sub-agent reconcile, Phase 2 sub-agent green, Phase 3 mechanical, Phase 4 review). Best fit because each task here is sized for one Phase 2 sub-agent dispatch.

**2. Inline Execution** — execute tasks in this session with checkpoints. Faster to start but ties up the orchestrator context with per-task implementation.

Which approach?
