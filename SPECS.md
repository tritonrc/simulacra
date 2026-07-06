# SPECS.md — Simulacra Behavioral Specifications

Specs define what the system must do. They are the source of truth for behavior.
Each spec lives in `specs/` and is testable. If code and spec disagree, the spec wins — fix the code.

## Index

| Spec | Status | Description |
|---|---|---|
| `specs/S001-vfs.md` | Active | Virtual filesystem: path resolution, chroot semantics, snapshot/restore |
| `specs/S002-shell.md` | Active | Shell emulator: builtins, pipes, redirects, exit codes |
| `specs/S003-quickjs.md` | Active | QuickJS runtime: module bindings, host function contracts, side-effect mediation |
| `specs/S004-capability.md` | Active | Capability tokens: structure, checking, attenuation on sub-agent spawn |
| `specs/S005-journal.md` | Active | Journal: entry types, checkpoint format, replay semantics, fork-from-checkpoint |
| `specs/S006-budget.md` | Active | Resource budgets: metering, enforcement, graceful exhaustion |
| `specs/S007-provider.md` | Active | Provider trait: request/response contract, streaming, retry, budget integration |
| `specs/S008-mcp.md` | Active | MCP integration: HTTP/SSE only, tool schema bridging, capability gating |
| `specs/S009-supervisor.md` | Active | Agent supervisor: spawn, cancel, restart strategies, message priority |
| `specs/S010-otel.md` | Reference | Observability conventions: span schemas, metric names, log levels (not a feature spec — o11y assertions live in each spec) |
| `specs/S011-sandbox-composition.md` | Active | Sandbox proxy layer: Golden Rule enforcement, AgentCell composition of VFS + Shell + QuickJS |
| `specs/S012-builtin-tools.md` | Active | Built-in tools: file_read, file_write, apply_patch, shell_exec, js_exec, list_dir |
| `specs/S013-cli.md` | Active | CLI: argument parsing, config loading, runtime wiring, headless execution |
| `specs/S014-esm-modules.md` | Active | ESM modules: module loading, remote fetch, import resolution |
| `specs/S015-interactive.md` | Active | Interactive mode: REPL, streaming output, tool approval, session persistence |
| `specs/S016-native-modules.md` | Active | Native ModuleDef for simulacra: modules, Object.keys() fix, node shell alias |
| `specs/S017-skills.md` | Active | Skills system: progressive disclosure, Skill tool, VFS-backed skill prompts |
| `specs/S018-interactive-subagents.md` | Active | Interactive sub-agent spawning: spawn_agent tool, supervisor integration, child result flow |
| `specs/S019-activity-events.md` | Active | Activity events: real-time tool/agent/thinking visibility via ActivitySink, collapsible blocks |
| `specs/S020-vfs-host-mounts.md` | Active | VFS host mounts: mount points, path traversal defense, prompt heuristics |
| `specs/S021-fetch.md` | Active | WHATWG Fetch API: Headers, Blob, Request, Response, AbortController, fetch() in QuickJS |
| `specs/S022-shell-http.md` | Active | Shell HTTP builtins: curl (13 flags) and wget (8 flags) via simulacra-http control plane |
| `specs/S023-generic-subagents.md` | Active | Generic sub-agent spawning: inline system prompts, tier-based model selection, leaf workers |
| `specs/S024-mcp-streamable-http.md` | Active | MCP streamable HTTP: 2025-03-26 transport, auto-detect with legacy SSE fallback, session management |
| `specs/S025-wasm-tools.md` | Active | WASM tool hosting: wasmtime WASIp2, WIT interface, fuel metering, sandboxed execution |
| `specs/S026-governance-hooks.md` | Active | Governance hook pipeline: Rack-style middleware, JS runtime, tool/llm/spawn/http hooks |
| `specs/S027-js-agent-capabilities.md` | Active | JS agent capabilities: web globals (base64, URL, TextEncoder), simulacra:path, simulacra:crypto, fs completions |
| `specs/S028-python-engine.md` | Active | Monty Python engine: py_exec tool with sandboxed Rust-native Python execution, external function mediation |
| `specs/S029-agent-procfs.md` | Active | Agent procfs: virtual /proc filesystem exposing runtime state (identity, budget, capabilities, tools) as readable files |
| `specs/S030-agent-payments.md` | Active | Agent spend management: PaymentProvider trait, transparent 402 handling, virtual cards, payment governance hooks, budget integration |
| `specs/S031-api-server.md` | Active | Simulacra API server: native WebSocket/REST+SSE API, protocol adapters (A2A, AG-UI), OAuth2/OIDC + API key auth, namespace-based multi-tenancy |
| `specs/S032-event-triggers.md` | Active | Event triggers: webhook receivers, cron/schedule-based task creation, EventSource trait for pluggable event sources |
| `specs/S033-integration-fabric.md` | Active | Integration fabric: credential management, `/svc/` VFS mount, `/var/skills/` namespace, credential injection |
| `specs/S034-simulacra-engine.md` | Active | SimulacraEngine: API-triggered agent execution, event bridging, per-task agent construction |
| `specs/S035-agent-worker-pool.md` | Active | Agent worker pool: bounded thread pool, backpressure, tenant integration scoping, task_status ownership |
| `specs/S036-task-files-and-artifacts.md` | Active | Task file attachments & artifact retrieval: file-in/file-out enterprise task lifecycle, VFS retention, enriched E2E scenarios |
| `specs/S037-memory-and-semantic-retrieval.md` | Active | Agent memory + RAG unified: /var/memory/ subtrees, SQLite vector store, local embeddings, semantic_search tool, virtual coworker MVP |
| `specs/S038-cli-memory-wiring.md` | Active | CLI memory wiring: [memory] TOML section, SqliteMemoryStore + BackgroundEmbedder bootstrap for entry agent, orderly shutdown drain, ensure_tenant fail-fast |
| `specs/S039-vfs-write-notifications.md` | Active | VFS write notifications: VirtualFs::subscribe(prefix) → VfsWatcher, VfsEvent broadcast, NotifyingFsLayer wrapper, Operation::VfsWrite governance hook |
| `specs/S040-wasm-backed-vfs-nodes.md` | Draft | WASM-backed VFS subtrees: WasmVfsLayer dispatching read/write/list_dir/remove/stat into WASM modules, capability gating, fuel/memory/duration metering |
| `specs/S041-wasm-mcp-servers.md` | Active | WASM MCP servers (Tier 2): in-process MCP via wasmtime component, hook-mediated `simulacra:http/fetch` for egress, same `mcp:{server}:{tool}` capability namespace as Tier 1, new `simulacra-mcp-server-sdk` for authoring |
| `specs/S042-agent-catalog-graphql.md` | Active (v1) | Agent catalog & GraphQL control plane: SQLite-backed `simulacra-catalog` (agents, skills, memory pools), `simulacra-graphql` async-graphql gateway, SimulacraEngine rewired to read from catalog, CatalogSkillFs VFS layer, one-shot TOML→DB import. v1 closes the catalog↔engine seam end-to-end; full o11y, CLI agent loop rewire, and provider-injection recording fixture deferred to follow-ups |
| `specs/S043-provider-injection-seam.md` | Active | Provider injection seam + stub-provider e2e: optional test-only `ProviderFactory` override on `SimulacraEngine` and a scripted `Provider` impl, closing S042 §E2E line 572 ("agent runs to completion") without requiring real LLM credentials |
| `specs/S044-graphql-tools-surface.md` | Active | GraphQL Tools surface: `Tool` type + `availableTools` query + `Agent.tools` derived field projecting `capabilities[]` into structured tools, so the agent-builder UI can render the Tools picker. Read-only; mutations stay on `updateAgent { capabilities }`. v1 lands the GraphQL schema + `ToolCatalog` trait; the `DefaultToolCatalog` impl wiring is deferred to its own commit |
| `specs/S045-per-agent-files.md` | Active | Per-agent files (static): `agent_files` catalog table + `AgentFileStore` trait + REST upload/download + GraphQL `Agent.files` / `detachAgentFile` + `CatalogAgentFileFs` mount at `/var/agent_files/`. Backs the Files section of the agent-builder form. Dynamic-file sources are S046+ |
| `specs/S046-channels.md` | Active | Channels v1: `channels` catalog table + `agent_channels` join + `ChannelRepository` + GraphQL `Channel` type, queries, mutations, `Agent.channels`. Backs the Channels multi-select in the agent-builder form. Runtime dispatch (Slack/Teams/Email/webhook routing) is S047+ |
| `specs/S048-simulacra-frontend.md` | Active (v1) | simulacra-frontend v1: new `simulacra-frontend` crate ships a no-compile Vue 3 SPA (native ESM + import maps, vue-router hash mode) plus a Rust shim mounted by `simulacra-server` via `Option<GraphQLMount>` in `build_router`. Three views — agent list (cards+drawer), agent form (two-column, Save & Run), agent run (dedicated route, activity feed + artifact sidebar). Backend deltas landed: `NoAuthProvider`/`NoAuthGraphQLProvider` for dev-mode bypass, mount glue, read-only `GET /api/v1/triggers`, `examples/dev_server.rs` manual-smoke entrypoint. Three assertions remain open and tied to the future production-CLI config path: `[server.frontend] enabled` toggle, `[server.auth] dev_mode` toggle, fail-fast on missing provider. Real auth, embedded assets, dynamic trigger CRUD, NL→scaffold are explicitly v2+ |
| `specs/S050-agent-streaming-runtime.md` | Active | Agent streaming runtime: provider streaming event contract, runtime activity deltas, deterministic replay fallback, cancellation handling, live tool-call input deltas, and server/frontend stream consumption |
| `specs/S051-agent-hitl-resume-runtime.md` | Active | Agent HITL resume runtime: input.response consumption, opt-in request_input tool, approval.respond resume, waiting task transitions, and replay-safe HITL behavior |
| `specs/S052-workflow-runtime.md` | Active | Workflow runtime: QuickJS-restricted orchestration scripts, persisted workflow runs, worker fan-out/resume/cancel, events, server routes, and Workflow tool |
| `specs/S053-async-quickjs-runtime-v2.md` | Active | Async QuickJS runtime v2: shared async substrate for js_exec and workflows, Promise-aware evaluation, static ESM prefetch, and restricted host profiles |
| `specs/S054-child-agent-orchestration.md` | Active | Child agent orchestration: status, bounded wait, and close tools for supervised child handles |
| `specs/S055-cli-jsonl-output.md` | Active | CLI headless JSONL output stream: `--output-format jsonl` emits the activity event stream as one envelope JSON object per line on stdout with a terminal `result` line, for programmatic/orchestrator consumption |
| `specs/S056-acp-child-agents.md` | Active | ACP-backed child agents as an alternate supervised child runtime behind the existing child-control tools, with ACP execution treated as an opaque injected runtime boundary |

## Spec Lifecycle

Specs are **living documents**. They grow as the system evolves. Individual assertions (`- [ ]` / `- [x]`) are the unit of progress, not the spec as a whole.

- **Draft:** Spec is a skeleton or design sketch. No testable assertions yet. Captured to record intent; not yet ready for implementation.
- **Active:** Spec has assertions. Some or all may be checked off. New assertions are added as work proceeds.
- **Reference:** Conventions or cross-cutting concerns (e.g. S010). Not a feature spec.
- **Stable:** Spec and implementation are settled. Changes require an ADR in `docs/decisions/`.

PM agents validate assertions against code/tests and check them off. Unchecked assertions represent known gaps.
