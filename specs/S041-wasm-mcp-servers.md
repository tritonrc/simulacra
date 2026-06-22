# S041 — WASM MCP Servers (Tier 2)

**Status:** Active
**Crates involved:** `simulacra-mcp`, `simulacra-wasm`, `simulacra-config`, `simulacra-hooks`, new `simulacra-mcp-server-sdk`

## Dependencies

- **ARCHITECTURE.md** — "MCP servers are accessed via HTTP/SSE (now) or WASM in-process (future)"
- **S008** — MCP integration (capability namespace, journal, OTel — all reused)
- **S024** — MCP streamable HTTP (`TransportMode` enum is the extension point)
- **S025** — WASM tool hosting (wasmtime, WASIp2, fuel metering, WASI sandbox — reused unchanged)
- **S026** — Governance hooks (`Operation::HttpRequest` is the egress mediation point)
- **S027** — Simulacra host interface (journal/capability/span exports — *not yet built*; v1 journals at the McpManager call site, true in-module exports are a follow-up)

## Scope

A `.wasm` component can serve as an MCP server, registered alongside HTTP/SSE servers in `[[mcp.servers]]` with `transport = "wasm"`. Tools exported by the module are dispatched in-process via wasmtime; outbound HTTP from the module is mediated by a host-imported `simulacra:http/fetch` that runs through the existing governance hook pipeline.

The capability surface (`mcp:{server}:{tool}`), journal entries, and OTel attributes are identical to Tier 1. Agents do not know which transport is in use.

**In scope:**

1. New `TransportMode::Wasm { module_id, instance }` variant in `simulacra-mcp::TransportMode`.
2. `transport = "wasm"` accepted in `McpServerConfig`; reuses the existing `module` field for the `.wasm` path.
3. WIT world `simulacra:mcp/server@0.1.0` with exports `list-tools` / `call-tool` mirroring S025's `simulacra:tools/tool` shape.
4. Host-imported world `simulacra:http/fetch` providing a single `fetch(request) -> result<response, fetch-error>` function, mediated by `Operation::HttpRequest` hooks.
5. Capability gating at the WASM transport entry: same `mcp:{server}:{tool}` glob check as Tier 1.
6. Per-call fuel + WASI sandbox from S025 reused unchanged for WASM MCP server modules.
7. Lazy instantiation: `Component` compiled at startup, `Store` created per `call_tool`, dropped after — no shared state.
8. Journal entry per tool call: same `gen_ai.tool.message` events and `simulacra.tool.source = mcp:{server}` attribute as Tier 1.
9. New `simulacra-mcp-server-sdk` crate: `#[mcp_tool]` proc-macro, ergonomic Rust authoring that emits the WIT exports.
10. Sample fixture `echo-mcp.wasm` for tests.
11. Feature flag: gated behind the same `wasm` feature as S025.

**Out of scope:**

- MCP resources, prompts, notifications, sampling, completions (v1 is tools-only).
- `wasi:http` outbound — deliberately deferred. See *Context*.
- Compiling unmodified upstream MCP SDKs (TS via jco, Python via py2wasm). Authors use `simulacra-mcp-server-sdk` for v1.
- Module signing, registry, distribution, hot-reload, pre-compiled `.cwasm` caching.
- True in-module Simulacra host exports (`simulacra_journal_append`, `simulacra_capability_check`) — that's S027.
- Tier 1 ↔ Tier 2 fallback. A server is one or the other; misconfiguration fails fast at startup.

## Context

**Why now.** S025 lands the WASM hosting machinery. S008/S024 land MCP. The `simulacra:tools/tool` WIT world from S025 is *already* shaped like MCP at the tool layer — `tools/list ↔ list-tools`, `tools/call ↔ call-tool`. Tier 2 is mostly plumbing the two together.

**Why hook-mediated `simulacra:http/fetch` and not `wasi:http`.** Many MCP servers wrap external APIs (GitHub, Stripe, databases-via-REST) and need outbound HTTP. The two options:

1. **`wasi:http`** — WASIp2 standard, future-proof for upstream SDK targets, native streaming. Bypasses Simulacra's hook pipeline unless we implement a custom `WasiHttpView` bridge that intercepts every verb. Failure mode if a verb is missed: silent egress.
2. **`simulacra:http/fetch`** — proprietary WIT, single host import that routes through `Operation::HttpRequest` hooks. Same code path as `simulacra-http` (shell `curl`/`wget`) and `simulacra-fetch` (QuickJS `fetch`). One HTTP egress story across the runtime. Failure mode if the import isn't linked: compile error inside the module.

V1 picks (2) to preserve the system's existing posture: every side effect goes through the host, every byte is journaled at one shape. `wasi:http` (or a hybrid that routes wasi-http internally through hooks) is a follow-up once (a) we want to host unmodified upstream MCP servers and (b) jco/py2wasm have stable WASIp2 outbound HTTP targets.

**Why one `McpManager`, not two.** Transport is an implementation detail; tool dispatch, capability checks, journal entries, OTel attributes, and reconnection (where applicable) should not branch on it. `TransportMode` already accommodates HTTP, SSE, and streamable HTTP — Wasm slots in the same way.

**Why `simulacra-mcp-server-sdk` is a new crate.** Authoring a Simulacra WASM MCP server should be `cargo new && add #[mcp_tool] && cargo build --target wasm32-wasip2`. The macro emits the WIT export glue and generates the `tool-def` schemas from Rust types; the runtime stays uninvolved.

## Design

### Config

`McpServerConfig` is unchanged at the type level — `transport`, `url`, and `module` are already optional. A WASM MCP entry looks like:

```toml
[[mcp.servers]]
name = "github"
transport = "wasm"
module = "tools/github-mcp.wasm"
fuel = 10_000_000

# Outbound HTTP allowlist for this module. Enforced by simulacra:http/fetch host
# function; per-request governance hooks may further deny.
network = ["api.github.com:443"]

# WASI sandbox config — same shape as [wasm.tools.wasi] in S025.
[mcp.servers.wasi]
fs = []
env = ["GITHUB_TOKEN"]
```

The `network` and `wasi` fields are added to `McpServerConfig` as optional. `McpServerConfig` validation rules:

- `transport = "wasm"` requires `module` to be set; `url` must be absent.
- `transport ∈ {"http", "sse"}` or absent (auto-detect): `url` required; `module` must be absent.

### Transport mode

`simulacra-mcp::TransportMode` gains a variant:

```rust
enum TransportMode {
    StreamableHttp { session_id: Option<String> },
    LegacySse { post_endpoint: String, sse_handle: JoinHandle<()> },
    Wasm { module_id: String, component: Arc<wasmtime::component::Component> },
}
```

The `Component` is compiled once at handshake. The `Store` is created per `call_tool`.

### WIT world

```wit
// wit/simulacra-mcp-server.wit

package simulacra:mcp@0.1.0;

interface types {
    record tool-def {
        name: string,
        description: string,
        input-schema: string,
    }

    variant tool-error {
        invalid-arguments(string),
        execution-failed(string),
    }
}

interface http {
    record request {
        method: string,
        url: string,
        headers: list<tuple<string, string>>,
        body: list<u8>,
    }

    record response {
        status: u16,
        headers: list<tuple<string, string>>,
        body: list<u8>,
    }

    variant fetch-error {
        capability-denied(string),
        hook-denied(string),
        transport(string),
        timeout,
    }

    fetch: func(req: request) -> result<response, fetch-error>;
}

world server {
    use types.{tool-def, tool-error};

    import http;

    export list-tools: func() -> list<tool-def>;
    export call-tool: func(name: string, arguments: string) -> result<string, tool-error>;
}
```

The `types` interface is identical to S025's `simulacra:tools/tool` to keep the SDK code path simple. The `http` interface is the v1 surface for outbound HTTP.

### Tool discovery

On handshake (lazy, first tool access — same trigger as Tier 1):

1. Compile `Component::from_file(module_path)` if not cached.
2. Instantiate with no fuel, call `list-tools()`.
3. Bridge each `tool-def` to a Simulacra `ToolDefinition`. Validate `input-schema` parses as JSON; warn-and-continue on invalid schemas (matches S025 behavior).
4. Mark `TransportMode::Wasm` and store the compiled component.
5. Register tools in the same path as Tier 1 — `mcp:{server}:{tool}` capability namespace.

### Tool dispatch

On `call_tool(server, tool, args)`:

1. Capability check (`mcp:{server}:{tool}` glob match against agent's capabilities). Same code as Tier 1.
2. Create fresh `Store` with WASI ctx from `[mcp.servers.wasi]` config and fuel from `min(server_fuel, agent_remaining_fuel)`.
3. Instantiate component via bindgen-generated linker; the linker injects:
   - `simulacra:mcp/types` (no host-side state)
   - `simulacra:mcp/http` host functions (see *Outbound HTTP*)
4. Call `call-tool(name, arguments_json)`.
5. On `Ok(json)` → return as `serde_json::Value`.
6. On `Err(invalid-arguments | execution-failed)` → `ToolError::ExecutionFailed(msg)`.
7. On `Trap::OutOfFuel` → `ToolError::ExecutionFailed("fuel exhausted")`.
8. On any other trap → `ToolError::ExecutionFailed(trap.to_string())`.
9. Record fuel consumed in agent budget (same path as S025).
10. Drop the `Store`. No state persists between calls.

### Outbound HTTP (`simulacra:http/fetch`)

Implemented as a host function in `simulacra-mcp` (not `simulacra-fetch`, to keep the dependency direction clean):

```rust
fn fetch(
    ctx: &mut WasmMcpHostCtx,
    req: http::Request,
) -> Result<http::Response, http::FetchError> {
    // 1. Capability check against [mcp.servers.network] allowlist.
    let host_port = parse_host_port(&req.url)?;
    if !ctx.network_allowlist.matches(&host_port) {
        return Err(http::FetchError::CapabilityDenied(host_port));
    }

    // 2. Run before-hook pipeline for Operation::HttpRequest.
    let verdict = ctx.hooks.run(Operation::HttpRequest, Phase::Before, &req)?;
    if verdict.is_deny() {
        return Err(http::FetchError::HookDenied(verdict.reason));
    }
    let req = verdict.transform(req); // hooks may redact headers/body

    // 3. Dispatch via shared reqwest client (same client that powers
    //    simulacra-http, simulacra-fetch — TLS, proxy, mTLS configured once).
    let resp = ctx.http_client.execute(req)?;

    // 4. Run after-hook pipeline; hooks may redact response body before
    //    handing back to the WASM module.
    let resp = ctx.hooks.run(Operation::HttpRequest, Phase::After, &resp)?;

    // 5. Journal the call.
    ctx.journal.append(JournalEntry::McpHttp { server, request, response });

    Ok(resp)
}
```

The reqwest client is shared with `simulacra-http` and `simulacra-fetch` so enterprise HTTP knobs (corporate proxy, custom CA, mTLS, redaction policy) configure once.

Per-request timeout: 30s (matches Tier 1 tool call timeout). Configurable via `[mcp.servers.timeout_ms]` (future).

### Networking allowlist semantics

`network` in `McpServerConfig` is a list of `host:port` patterns:

- `"api.github.com:443"` — exact host, exact port.
- `"*.stripe.com:443"` — host glob, exact port.
- `"localhost:*"` — exact host, any port.
- Empty or missing list → no outbound HTTP allowed (default-deny).

Pattern matching is implemented in `simulacra-mcp` (not `simulacra-tool` glob_match) because the syntax is `host:port`, not capability-token shape.

### Authoring SDK (`simulacra-mcp-server-sdk`)

```rust
use simulacra_mcp_server_sdk::{mcp_tool, fetch};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, schemars::JsonSchema)]
struct CreateIssueArgs {
    repo: String,
    title: String,
    body: String,
}

#[derive(Serialize)]
struct Issue { id: u64, html_url: String }

#[mcp_tool(description = "Create a GitHub issue")]
fn create_issue(args: CreateIssueArgs) -> Result<Issue, String> {
    let token = std::env::var("GITHUB_TOKEN").map_err(|e| e.to_string())?;
    let resp = fetch::post(
        format!("https://api.github.com/repos/{}/issues", args.repo),
        &serde_json::json!({ "title": args.title, "body": args.body }),
        &[("Authorization", &format!("Bearer {}", token))],
    ).map_err(|e| format!("{:?}", e))?;
    serde_json::from_slice(&resp.body).map_err(|e| e.to_string())
}
```

The macro:
- Generates `list-tools()` from the set of `#[mcp_tool]` functions in the crate.
- Derives `input-schema` from the arg type via `schemars`.
- Generates `call-tool(name, args_json)` dispatch table.
- `fetch::*` helpers wrap the imported `simulacra:mcp/http.fetch`.

### Crate position

```
simulacra-types (leaf)
  └→ simulacra-wasm (wasmtime, wasmtime-wasi, simulacra-types)
       └→ simulacra-mcp (reqwest, simulacra-types, simulacra-wasm [feature], simulacra-hooks)
            └→ simulacra-cli (optional: feature "wasm")

simulacra-mcp-server-sdk (separate workspace member, NOT depended on by runtime crates)
  └→ wit-bindgen, schemars, serde_json
```

`simulacra-mcp` gains a `wasm` feature that pulls in `simulacra-wasm`. Default builds keep the existing HTTP/SSE-only path.

`simulacra-mcp-server-sdk` is for *authors* of WASM MCP servers; it is not a runtime dependency.

## Behavior

### Config loading

1. `transport = "wasm"` is accepted and parsed into a config-time enum value.
2. `transport = "wasm"` without `module` is a config error.
3. `transport = "wasm"` with `url` set is a config error.
4. `network` defaults to an empty list (no outbound HTTP).
5. `[mcp.servers.wasi]` defaults to empty fs/env (same as S025 default).

### Module loading

6. On handshake, the `.wasm` file is compiled to a `Component` and cached.
7. Compilation failure returns `McpError::ConnectionFailed { reason: "wasm compile failed" }`.
8. `list-tools()` is called once per module instantiation at handshake.
9. Tools are registered in `ToolRegistry` under `mcp:{server}:{tool}` capability namespace.

### Capability gating

10. `call_tool` checks `mcp:{server}:{tool}` against the agent's capabilities before instantiating the WASM component. Identical to Tier 1.
11. Capability denial returns `McpError::CapabilityDenied`. No WASM instantiation occurs.

### Tool execution

12. Each `call_tool` creates a fresh `Store` with a new WASI context and fuel limit.
13. Fuel limit is `min(server_fuel, agent_remaining_fuel)` (0 = unlimited, same as S025).
14. The `Component` is shared across calls; only the `Store` is per-call.
15. `call-tool(name, args_json)` is invoked on the instance.
16. On success, the JSON string return is parsed into `serde_json::Value`.
17. `Trap::OutOfFuel` produces `ToolError::ExecutionFailed("fuel exhausted")`.
18. `tool-error::invalid-arguments` produces `ToolError::ExecutionFailed`.
19. `tool-error::execution-failed` produces `ToolError::ExecutionFailed`.
20. The `Store` is dropped after each call. No state persists.

### Outbound HTTP from the module

21. `simulacra:http/fetch` checks `host:port` against the per-server `network` allowlist before any other action.
22. Allowlist denial returns `fetch-error::capability-denied(host_port)` to the WASM module.
23. On allowlist pass, the request runs through `Operation::HttpRequest` `Phase::Before` hooks.
24. Hook denial returns `fetch-error::hook-denied(reason)` to the WASM module.
25. Hooks may transform (redact) request headers and body before dispatch.
26. The shared reqwest client executes the request.
27. The response runs through `Operation::HttpRequest` `Phase::After` hooks before being returned.
28. Hooks may transform (redact) response headers and body before handing back to the WASM module.
29. A journal entry is written for every fetch call (success and failure).
30. Per-request timeout is 30s; timeout returns `fetch-error::timeout`.
31. No outbound HTTP is reachable except via `simulacra:http/fetch`. WASI networking remains disabled.

### What doesn't change

32. `McpManager` public API (`connect`, `list_tools`, `call_tool`) is unchanged.
33. The `mcp:{server}:{tool}` capability namespace is unchanged.
34. `gen_ai.tool.message` events and `simulacra.tool.source = mcp:{server}` attributes are unchanged.
35. Reconnection backoff (S008) does not apply to WASM MCP servers — there is no transport to reconnect.
36. Lazy connection semantics (S008) apply: the module is not compiled until first tool access.

## Assertions

### Config

- [ ] `transport = "wasm"` is accepted by `McpServerConfig` parsing.
- [ ] `transport = "wasm"` without `module` returns a typed config error.
- [ ] `transport = "wasm"` with `url` set returns a typed config error.
- [ ] `network` field defaults to an empty list when omitted.
- [ ] `[mcp.servers.wasi]` parses with the same shape as S025's `[wasm.tools.wasi]`.

### Module loading

- [ ] Handshake compiles the `.wasm` file into a `Component` and caches it.
- [ ] Compile failure produces `McpError::ConnectionFailed`.
- [ ] `list-tools()` is called once at handshake and produces valid `ToolDefinition`s.
- [ ] Tools are registered under `mcp:{server}:{tool}` namespace (same as Tier 1).
- [ ] A module exporting multiple tools registers multiple `ToolDefinition`s.

### Capability gating

- [ ] `call_tool` with a tool outside the agent's capabilities returns `CapabilityDenied` without instantiating the component.
- [ ] Glob capability `mcp:*:*` allows all WASM MCP tool calls.
- [ ] Capability check happens before WASI context creation (no side effects on denial).

### Tool execution

- [ ] Each `call_tool` creates a fresh `Store` (verified by mutating module-local state across calls and observing it is reset).
- [ ] `call-tool("echo", ...)` on the fixture returns the expected JSON.
- [ ] `call-tool("nonexistent", ...)` returns `ToolError::ExecutionFailed`.
- [ ] Fuel exhaustion returns `ToolError::ExecutionFailed("fuel exhausted")`.
- [ ] Agent fuel budget exhaustion fails the call without instantiating the component.
- [ ] Fuel consumed is subtracted from the agent's `ResourceBudget`.

### Outbound HTTP

- [ ] `fetch` to a host outside the `network` allowlist returns `capability-denied`.
- [ ] `fetch` to a `host:*` allowlist entry permits any port for that host.
- [ ] `fetch` to a `*.example.com:443` entry permits subdomain matches at port 443.
- [ ] An empty `network` list rejects all outbound HTTP.
- [ ] `Operation::HttpRequest` `Phase::Before` hook is invoked before the wire dispatch.
- [ ] A `Phase::Before` deny verdict returns `hook-denied(reason)` to the module.
- [ ] A `Phase::Before` redact verdict modifies headers/body before dispatch.
- [ ] `Operation::HttpRequest` `Phase::After` hook is invoked after the response and before returning to the module.
- [ ] A `Phase::After` redact verdict modifies headers/body before handing back to the module.
- [ ] Every `fetch` call (success and failure) writes a journal entry.
- [ ] WASI networking is disabled — `wasi:sockets` calls inside the module fail.
- [ ] Request timeout of 30s returns `fetch-error::timeout`.

### Authoring SDK

- [ ] A crate using `#[mcp_tool]` compiles to a WASIp2 component when targeting `wasm32-wasip2`.
- [ ] The compiled component's `list-tools()` returns one entry per `#[mcp_tool]` function.
- [ ] `input-schema` is derived from the arg type via `schemars` and parses as JSON Schema.
- [ ] `call-tool(name, args)` dispatches to the right function and serializes the return.
- [ ] Calls to `fetch::*` helpers route through the imported `simulacra:mcp/http.fetch`.

### What doesn't change (regression guards)

- [ ] `McpManager::call_tool` signature is unchanged. (A new sibling
  method `call_tool_for_agent(agent_id, ...)` is added alongside it for
  shared-process / multi-agent deployments — see § Per-agent journal
  attribution below. The existing `call_tool` is retained verbatim and
  delegates to `call_tool_for_agent` with an empty `AgentId`.)
- [ ] `gen_ai.tool.message` events for WASM MCP servers carry `simulacra.tool.source = mcp:{server}` (same as Tier 1).
- [ ] `simulacra.mcp.calls` counter is incremented for WASM MCP calls with `server` and `tool` labels.
- [ ] HTTP/SSE MCP servers continue to work unchanged (S008 + S024 assertions remain green).

### Per-agent journal attribution

In single-agent processes (e.g. `simulacra-cli`), one `McpManager` lives per
process and the agent identity is implicit. In shared-process deployments
(e.g. `simulacra-server`), one `McpManager` is reused across many concurrent
agents — connection pools, cached components, and capability checks are
all shared per-process — but each outbound `simulacra:mcp/http.fetch` audit
entry must still be attributed to the agent that triggered it.

- [ ] `McpManager::call_tool_for_agent(agent_id, server, tool, args, capability)` exists.
- [ ] Each `simulacra:mcp/http.fetch` journal entry written during the call
      carries the per-call `agent_id`.
- [ ] When `agent_id` is empty, the dispatch path falls back to the
      `WasmMcpModule`'s bake-in `agent_id` (CLI back-compat).
- [ ] When `agent_id` is non-empty, it overrides any module bake-in
      default (server requirement).

## Observability (see S010)

- [ ] `simulacra_mcp_handshake` span includes `simulacra.mcp.transport_mode = "wasm"` for WASM MCP servers.
- [ ] `simulacra_mcp_handshake` span includes `simulacra.mcp.module_id` for WASM MCP servers.
- [ ] `simulacra_mcp_tool_call` span for WASM MCP servers includes `simulacra.wasm.fuel_consumed`.
- [ ] `simulacra.wasm.fuel_consumed` histogram records fuel per call with `module` and `tool` labels (same as S025).
- [ ] `simulacra_mcp_http_fetch` span wraps each outbound `fetch` call with `http.method`, `http.url.host`, `http.response.status_code`.
- [ ] `simulacra.mcp.http.denied` counter incremented on `capability-denied` or `hook-denied`, with `server` and `reason` labels.
- [ ] `tracing::warn!` on hook denial inside `simulacra:http/fetch`.
- [ ] `tracing::error!` on WASM trap during `call-tool`.

## Open questions

1. **Per-server vs shared reqwest client.** *Partially resolved.*
   `WasmMcpModule` now owns one `reqwest::Client` per module, cloned
   into each per-call store so connection-pool / proxy / TLS config is
   shared across all fetches from the same module.
   `WasmMcpModule::with_http_client(client)` lets enterprise deployments
   thread in a centrally-configured client (e.g. one ultimately produced
   by `simulacra-http`'s async surface). The cross-transport story —
   sharing one client across HTTP MCP, WASM MCP fetch, shell `curl`,
   and QuickJS `fetch` — remains open and tracks against `simulacra-http`'s
   roadmap.
2. **Streaming responses.** v1 returns the full body as `list<u8>`. Real-world MCP servers wrapping streaming APIs (LLM proxies, log tailing) need chunked bodies. Adding `incoming-body` / `outgoing-body` resources to the WIT later is non-breaking; v1 punts.
3. **Resources / prompts / notifications.** Tools-only is a deliberate v1 cut. Resources are the next-most-valuable; prompts/notifications/sampling are smaller. Each is a follow-up spec.
4. **Module hot-reload.** S025 doesn't support it; v1 doesn't either. A registry / hot-reload story is a separate spec.
5. **Distribution.** `module = "tools/github-mcp.wasm"` is a file path. A signed registry with version pinning is the obvious follow-up; deliberately out of v1.
6. **`simulacra_journal_append` / `simulacra_capability_check` host imports (S027).** v1 journals at the McpManager call site. If S027 lands first, the WASM MCP server module can call into the journal directly for richer audit trails inside the module's own code paths.
7. **Hybrid: implement `wasi:http` *and* route it through hooks internally.** Possible follow-up if upstream MCP SDKs (jco, py2wasm) stabilize their WASIp2 outbound HTTP targets.
