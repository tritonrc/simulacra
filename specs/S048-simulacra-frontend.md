# S048 — simulacra-frontend (v1)

> **Status:** Draft
>
> **Related:** S031 (api server), S042 (agent catalog + GraphQL), S044 (tools surface), S045 (per-agent files), S046 (channels), S019 (activity events), S036 (task artifacts)
>
> **What this unblocks:** the agent-builder form — the immediate UX northstar — runs end-to-end in a browser. Use cases 1–4 (create, list+view, edit, run) plus a read-only triggers view.

## Summary

A new crate `simulacra-frontend` ships a no-compile Vue 3 SPA as static assets, plus a thin Rust shim exposing `frontend_router() -> axum::Router`. `simulacra-server::build_router` merges that router alongside `simulacra_graphql::graphql_router(...)` so a single `simulacra` binary serves the UI, the GraphQL gateway, and the existing REST control plane on one origin. v1 is a vertical spike: dev-mode auth bypass, hash routing, ServeDir asset serving — production polish (real auth, embedded assets, browser regression tests) is deferred.

The split:
- **v1 (this spec):** crate + mount glue, agent list (cards-with-drawer), agent form (two-column, Save & Run), agent run view (dedicated route, activity feed + artifact sidebar), read-only triggers display, dev-mode auth bypass, `GET /api/v1/triggers` endpoint.
- **v2+ (later specs):** real auth flow (OIDC or API-key login screen), `include_dir!` asset embedding, browser end-to-end tests, dynamic trigger CRUD UI (which itself depends on a separate trigger-catalog spec), inline-agent run path.

## Authority

- `ARCHITECTURE.md`: agents are catalog-resident; tasks run against named agents.
- `feedback_no_compile_frontend` (user memory): no bundler, no transpile, no JSX, no SFCs. Native ESM + import maps only.
- `project_northstar_agent_form` (user memory): form sections are Title · Channels · Tools · Skill · Files · Instructions, plus Save / Save & Run buttons.
- `S042 §GraphQL`, `S044 §GraphQL`, `S045 §REST + GraphQL`, `S046 §GraphQL`: the surface the form consumes is already specified and shipped.

## Out of scope (v1)

- No real authentication. v1 ships a `NoAuthProvider` gated by a `dev_mode` config flag.
- No production asset bundling. ServeDir over the crate's `assets/` directory; `include_dir!` is v2+.
- No NL→scaffold "what should your agent do?" flow.
- No dynamic trigger CRUD. Triggers are read-only in v1, sourced from the existing TOML config.
- No inline-agent run path. "Run while creating" = Save then Run (two API calls, one button).
- No multi-tenant UI affordances (tenant switcher, per-tenant theming). The dev identity defines the tenant.
- No browser end-to-end tests (no Playwright / Cypress). Composable unit tests + Rust integration test on the mount only.
- No history-mode routing. Hash routing only (`createWebHashHistory`); no SPA fallback in ServeDir.
- No agent performance / observability dashboards (use case 6 — explicitly deferred).

## Architecture

```
crates/simulacra-frontend/
  Cargo.toml
  src/lib.rs                              pub fn frontend_router() -> axum::Router
  assets/                                 served verbatim by ServeDir
    index.html                            <script type="importmap"> + boot
    main.js                               Vue app + vue-router (hash mode)
    api/
      graphql.js                          POST /graphql wrapper
      rest.js                             /api/v1/* fetch wrapper
      sse.js                              EventSource wrapper for /api/v1/tasks/:id/events
    composables/
      useAgents.js                        list / get / create / update / saveAndRun
      useChannels.js                      list / create
      useTools.js                         availableTools query
      useSkills.js                        skills query
      useAgentFiles.js                    multipart upload + detach
      useTriggers.js                      GET /api/v1/triggers?agent=:id
      useTaskStream.js                    SSE → reactive events[] + status
      useTaskArtifacts.js                 list + download URLs
    components/
      app-shell.js                        nav + route outlet
      agent-list.js                       card grid + drawer
      agent-form.js                       two-column form (use cases 1 + 3)
      agent-run.js                        activity feed + artifact sidebar
      pickers/
        channel-picker.js, tool-picker.js,
        skill-picker.js, file-uploader.js,
        trigger-list.js                   read-only
      activity/
        event-token.js, event-thinking.js,
        event-tool-call.js, event-child.js,
        artifact-sidebar.js
    styles.css
  tests/
    frontend_mount.rs                     boots router, GET /index.html → 200
```

### Backend deltas

The frontend cannot work without these small backend additions, which are part of this spec's scope:

1. **NoAuthProvider** — `simulacra-server::auth::NoAuthProvider` and `simulacra-graphql::auth::NoAuthGraphQLProvider`. Both implement their respective `AuthProvider` traits, returning a fixed `Identity` / `AuthPrincipal` configured at construction. Used when `simulacra.toml`'s `[server.auth]` section sets `dev_mode = true`. Production deploys leave `dev_mode = false` (or unset) and configure a real provider.

2. **Mount glue** — `simulacra-server::build_router` merges `simulacra_frontend::frontend_router()` (ServeDir at `/`) and `simulacra_graphql::graphql_router(...)` (POST `/graphql`). Order matters: API routes (`/api/v1/*`, `/graphql`) precede the static fallback so the frontend doesn't shadow them.

3. **Triggers read endpoint** — `GET /api/v1/triggers?agent=:agent_type` returns the configured webhooks and schedule entries (from `AppState.webhooks` + `Scheduler`) that target that agent. Read-only; mirrors the existing `[[webhooks]]` / `[[schedules]]` TOML config. No filtering = return all triggers for the resolved tenant.

4. **`examples/dev_server.rs`** in `simulacra-server` — a runnable example that boots the full router with `NoAuthProvider`, `frontend_router()`, `graphql_router()`, and a small in-memory catalog seeded with one example agent. This is the v1 manual-smoke entrypoint. Production CLI wiring (`simulacra serve` with config-driven auth provider selection) is a separate follow-up spec.

## Routing

```
#/                              → AgentList     (use case 2)
#/agents/new                    → AgentForm     (use case 1, blank)
#/agents/:id                    → AgentForm     (use case 3, populated)
#/agents/:id/run/:taskId        → AgentRun      (use case 4)
```

The card drawer is local UI state (a `selectedAgentId` ref), not a route. The "Run" action from list view first POSTs to `createTask`, then `router.push` to the run route once the task id comes back. "Save & Run" from the form does `mutation { createAgent | updateAgent }` → `POST /api/v1/tasks/create` → `router.push`.

## Data flow

Each composable is a module-scoped Vue 3 setup function returning `{ data, loading, error, refresh, ...mutators }`. Composables own all I/O; components import composables, never `api/*` directly. Mutations refresh the relevant composable's cache on success. No global store; module-scoped reactive state suffices for v1.

`useTaskStream(taskId)` is the only long-lived stateful composable. It opens an `EventSource`, parses each SSE message into a typed event variant (Token / ThinkStart / ThinkDelta / ToolCallDelta / ToolStart / ToolEnd / ChildActivity / TaskComplete / Error), pushes onto a reactive `events[]`, and tears down on `onUnmounted`. One reconnect attempt on transport drop, then surfaces "stream interrupted."

## Error handling

- **Network/transport errors:** `api/*` wrappers throw typed errors; composables expose them via `error` ref; `app-shell` shows a top-of-viewport toast. SSE reconnects once before surfacing.
- **GraphQL field errors / 4xx with structured body:** rendered inline next to the offending form field. No toast.
- **Unexpected 5xx / invalid JSON:** toast + console log; component renders an empty/error state but doesn't crash the route.
- **SSE 4xx on run page:** dedicated empty state ("task not found / not authorized").

No global error boundary. Vue's default per-component error logging is acceptable for an internal tool.

## Testing

- **Composable unit tests** under `assets/composables/*.test.mjs`, run via `node --test`. Mock `fetch` globally. Verify each composable shapes responses, refreshes after mutations, surfaces errors.
- **Rust integration test** in `crates/simulacra-frontend/tests/frontend_mount.rs`: boots the router, hits `/index.html` → 200 + `text/html` content-type.
- **End-to-end mount test** in `crates/simulacra-server/tests/`: boots the full router with `frontend_router()` + `graphql_router()` mounted with `NoAuthProvider`, sends `query { agents { id } }` to `/graphql`, gets `{"data":{"agents":[]}}`. Confirms cross-crate wiring works.

No browser-driving tests. Visual surface validated manually against the dev server.

## Configuration

`simulacra.toml` gains:

```toml
[server.auth]
dev_mode = true            # default: false. Enables NoAuthProvider.
dev_identity = "dev@local" # subject string for the synthetic identity
dev_tenant = "default"     # tenant namespace for the synthetic identity

[server.frontend]
enabled = true             # default: true. When false, frontend_router() is not mounted.
```

When `dev_mode = false` AND no real auth provider is configured, the server refuses to start (no silent unauthenticated mode in production).

## Assertions

### Crate scaffolding
- [ ] `crates/simulacra-frontend/Cargo.toml` exists with workspace inheritance and depends on `axum`, `tower-http` (with `fs` feature).
- [ ] `simulacra_frontend::frontend_router()` returns an `axum::Router` that serves `assets/` via `ServeDir`.
- [ ] `GET /` returns `index.html` with `text/html` content-type and 200.
- [ ] `GET /styles.css` returns 200 with `text/css` content-type.
- [ ] `GET /unknown.js` returns 404 (no SPA fallback in v1).

### Mount glue
- [ ] `simulacra-server::build_router` mounts `frontend_router()` and `graphql_router(...)` when `[server.frontend] enabled = true`.
- [ ] `POST /graphql` reaches the GraphQL handler (not the static fallback).
- [ ] `POST /api/v1/tasks/create` reaches the existing REST handler (not shadowed by `frontend_router`).
- [ ] Disabling `[server.frontend]` removes the static routes; `/graphql` and `/api/v1/*` still work.

### NoAuthProvider
- [ ] `NoAuthProvider::authenticate` returns the configured `Identity` regardless of credentials.
- [ ] `NoAuthGraphQLProvider::authenticate` returns the configured `AuthPrincipal` regardless of headers.
- [ ] When `dev_mode = false` and no other provider is set, `simulacra-server` startup fails with a clear "no auth provider configured" error.

### Triggers endpoint
- [ ] `GET /api/v1/triggers?agent=:agent_type` returns `{ webhooks: [...], schedules: [...] }` filtered to triggers targeting that agent for the caller's tenant.
- [ ] `GET /api/v1/triggers` (no filter) returns all triggers for the caller's tenant.
- [ ] Cross-tenant triggers are not returned.
- [ ] Webhook entries include `path`, `agent_type`, and HMAC-presence flag (no secret leak).
- [ ] Schedule entries include `cron`, `agent_type`, and `missed_policy`.

### Composables (browser-side, `node --test` with mocked fetch)
- [ ] `useAgents.list()` issues a single GraphQL `agents { ... }` query and exposes `data` as a reactive ref.
- [ ] `useAgents.create(input)` calls `createAgent`; on success, `list()` cache is invalidated.
- [ ] `useAgents.saveAndRun(input)` calls create-or-update then `POST /api/v1/tasks/create` and resolves with the `task_id`.
- [ ] `useTaskStream(taskId)` opens an `EventSource`, pushes parsed events onto `events[]`, sets `status` to `completed` on terminal event.
- [ ] `useTaskStream` tears down the `EventSource` on `onUnmounted`.
- [ ] `useTaskStream` attempts one reconnect on transport drop, then surfaces `error = "stream interrupted"`.
- [ ] `useTriggers(agentId)` calls `GET /api/v1/triggers?agent=:id` and exposes the response.
- [ ] Composable errors do not throw out of the function; they are captured into the `error` ref.

### Manual smoke (not protocol-asserted in v1)

These behaviors are validated by hand against `cargo run -p simulacra-server --example dev_server` (a new example added in this spec — production `simulacra serve` CLI wiring is its own follow-up). Browser regression tests that turn them into automated assertions are explicitly v2+. They are listed here so v1 acceptance is unambiguous, not because they are testable from Rust:

- `#/` renders the agent list with a card grid and the "+ New agent" card.
- Clicking a card opens a drawer showing the agent's full composition with Edit and Run buttons.
- `#/agents/new` renders the form with empty fields and Save / Save & Run buttons.
- `#/agents/:id` renders the form with fields populated from the agent, including the read-only triggers section in the meta column.
- `#/agents/:id/run/:taskId` renders the activity feed + artifact sidebar; events stream in real time; artifacts populate as the task writes them.
- Save returns to the list view; Save & Run navigates to the run route.

### v2+ (deferred — NOT v1)
- Real auth (OIDC redirect, API-key login screen).
- Embedded assets via `include_dir!`, with a `dev` cargo feature flag preserving ServeDir for development.
- Dynamic trigger CRUD UI (depends on a separate trigger-catalog spec adding `triggers` table + GraphQL mutations).
- Inline-agent run path (`POST /api/v1/tasks/create` accepts an inline `agent_spec` instead of `agent_type`).
- Activity-event UI polish (collapsible groups, syntax highlighting for tool args, child-agent nesting depth).
- Browser end-to-end regression tests.
- NL→scaffold "what should your agent do?" agent (its own spec).

## Why this is shippable on its own

- The form's GraphQL/REST surface is already shipped (S042/S044/S045/S046/S031). v1's frontend code is a consumer.
- Backend deltas are minimal and bounded: two `AuthProvider` impls, one router merge, one read-only endpoint.
- Auth is honest dev-mode bypass, not a half-finished real auth flow. Production deploys cannot start in dev mode without explicit opt-in.
- The form, list, and run views are coherent on their own — none of them depends on a feature that isn't built.
- Everything deferred to v2+ has an explicit follow-up path; nothing is left dangling.
