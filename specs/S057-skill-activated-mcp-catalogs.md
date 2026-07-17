# S057 — Skill-Activated MCP Catalogs

**Status:** Active
**Crates involved:** `simulacra-config`, `simulacra-cli`, `simulacra-runtime`, `simulacra-tool`, `simulacra-mcp`, `simulacra-types`

## Dependencies

- **ARCHITECTURE.md** — Golden Rule, host-side capability enforcement, journal-before-return, HTTP/SSE-or-WASM MCP boundary, and OTel conventions
- **S004** — capability tokens and attenuation
- **S005** — append-only journal entries before side-effect results return
- **S008** — MCP transport, `mcp:<server>:<tool>` capability namespace, retry behavior, and MCP call telemetry
- **S010** — observability conventions and local Aniani validation
- **S017** — VFS-backed skills, skill eligibility, `Skill` loading, and `allowed_tools` approval-only semantics
- **S018** — independent child agent capability attenuation and isolation
- **S024** — streamable HTTP/SSE MCP handshake behavior
- **S041** — WASM MCP transport, where configured

## Scope

Replace eager registration of one provider-visible tool schema per configured
MCP tool with two stable, direct meta-tools: `mcp_search` and `mcp_call`.

A skill may declare the configured MCP servers it needs in `SKILL.md`
frontmatter. Loading that skill automatically and atomically activates those
servers for that **agent session**. Activation performs the existing MCP
handshake and inventories the declared tools only then. Activated tools are
discoverable through `mcp_search`; a tool can be dispatched only through
`mcp_call` after that search has published it.

This keeps provider tool definitions stable for a session, avoids startup
connections and eager `tools/list`, and does not change MCP transport,
capability, hook, journal, retry, or call-observability behavior already
governed by S008/S024/S041.

The indexed-search follow-up in this spec changes only `simulacra-mcp`:
activated inventories are compiled into a session-local immutable inverted
index so repeated searches do not normalize or sort the full catalog. Full
tool schemas remain in the catalog and are cloned/serialized only for the
winning results. The provider-visible meta-tool contracts and all existing
activation, publication, call, and redaction boundaries remain unchanged.

## Non-Goals

- No provider-visible MCP schema per server or per MCP tool.
- No connection, credential exchange, or `tools/list` request merely because a
  server is configured.
- No activation of servers not declared by a successfully loaded skill.
- No dynamic activation bundle other than MCP server catalogs in v1.
- No grant of capabilities through `mcp_servers` or `allowed_tools`.
- No cross-agent shared activated catalog, even when transports or configured
  descriptors are shared internally.
- No change to the existing `Skill` tool name, skill-body format, or normal
  explicit user/model skill loading semantics except the activation behavior
  defined here.
- No cache of query results or retention of normalized query material after a
  search. The catalog index, not query caching, provides the optimization.
- No server or skill filters on `mcp_search`.
- No change to the provider-visible `mcp_search` or `mcp_call` names, input
  schemas, or result fields.

## Design

```text
Bootstrap
   |
   +--> retain authorized MCP server descriptors per agent
   |      (no connect; no tools/list; no provider MCP schemas)
   |
   +--> register stable mcp_search + mcp_call when MCP is configured
   |
Skill(command = "repo-work") or /repo-work
   |
   +--> validate skill and every declared mcp_server before network access
   |
   +--> handshake + inventory every newly declared server
   |      all succeed ----------------> commit per-agent activated catalog
   |                                     then return/inject skill body
   |      any failure ----------------> discard temporary catalog; return error
   |
   v
mcp_search(query) --> immutable session index snapshot
                  --> bounded top-5 ranked, activated schemas
   |
   v
mcp_call(server, tool, arguments) --> only a search-published activated tool
                                      --> existing mcp:<server>:<tool> dispatch
```

### Skill frontmatter

S017 frontmatter gains an optional `mcp_servers` field:

```yaml
---
name: repo-work
description: Work with repository issues and pull requests.
mcp_servers:
  - github
  - linear
allowed_tools:
  - file_read
---
```

- `mcp_servers` is an optional array of non-empty configured MCP server names.
  Omission means the skill has no MCP activation dependency.
- Names are canonical configured server names, not URLs, transport names, or
  arbitrary provider tool names. Duplicate names are normalized to one server
  dependency while preserving first-listing order.
- `allowed_tools` retains S017 meaning: it changes interactive approval only;
  it never grants MCP capability, makes a server eligible, connects a server,
  or authorizes `mcp_call`.

### Stable direct tools

When at least one MCP server is configured for the current runtime, the
provider-visible tool set includes exactly these fixed MCP meta-tools. Their
schemas and names do not vary with configured or activated MCP inventory.

#### `mcp_search`

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "query": { "type": "string", "description": "Terms used to rank activated MCP tools" }
  },
  "required": ["query"],
  "additionalProperties": false
}
```

**Result:** a bounded list of at most five matching tools. Every result includes
the configured `server` name, MCP `tool` name, description, and input schema.
Only tools from the calling agent's activated catalog are eligible. Ranking is
deterministic for identical catalog and query inputs; ties are ordered by server
then tool name. An empty query is valid and returns the first ranked five
activated tools; an empty activated catalog returns an empty successful result.

### Indexed catalog snapshot

Each successfully inventoried tool contributes index entries for its
normalized configured server name, tool name, and description. Normalization
lowercases text and splits it into tokens at non-alphanumeric boundaries. The
index stores lookup and ranking metadata separately from full input schemas so
searching non-winning candidates does not clone or serialize their schemas.

The activated catalog exposes one immutable, session-scoped snapshot containing
the committed inventories and their index. Search acquires that snapshot and
releases mutable catalog state before candidate lookup and ranking. A successful
activation builds the complete replacement snapshot before atomically
publishing it. A failed activation publishes nothing, and reactivation of an
already committed server neither duplicates inventory nor index entries.

For a non-empty query, normalization also removes duplicate terms. Every
distinct query term must match at least one indexed field for a tool, regardless
of query-term order. Reordering terms therefore preserves eligibility and the
base per-term relevance and tie behavior. A match is either an exact normalized
token or a prefix of an indexed token when the query term has at least three
characters; shorter terms never prefix-match.

Relevance aggregates each term's strongest field match in this descending
order: exact tool-name token, tool-name prefix, exact server-name token, server
name prefix, exact description token, then description prefix. The strongest
overall boost applies when the complete normalized query equals the complete
normalized tool name. This complete-name comparison preserves normalized query
order, so it may distinguish otherwise equivalent query-term permutations.
Equal relevance is resolved by configured server name, then tool name.
Empty-query results retain alphabetical server-then-tool order. Search uses a
bounded top-five selection structure rather than sorting all matches, then
clones/serializes schemas only for those selected results.

#### `mcp_call`

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "server": { "type": "string" },
    "tool": { "type": "string" },
    "arguments": { "description": "JSON value passed as MCP tool arguments" }
  },
  "required": ["server", "tool", "arguments"],
  "additionalProperties": false
}
```

`mcp_call` dispatches through the existing S008/S024/S041 MCP path only when
`(server, tool)` is both activated for this agent and previously returned by a
successful `mcp_search` in this agent session. Publication is scoped to the
exact `(server, tool)` pair, is idempotent, and survives later searches and
skill loads for the session. Unknown, unactivated, or not-search-published
pairs return an actionable error without a network tool call.

Before dispatch, `mcp_call` enforces the existing
`mcp:<server>:<tool>` capability check at the call site. It then preserves all
existing S008 behavior for hooks, journaling before return, transport retry,
spans, and errors. The raw arguments are forwarded unchanged to the MCP
dispatcher, but observability and journal records contain only safe metadata:
`server`, `tool`, and serialized argument length. `gen_ai.tool.message` input
events likewise contain argument length rather than the raw argument value. The
outer `AgentLoop` journal entry and `ToolStart` activity payload apply the same
projection before publication, so activity consumers and server SSE streams
never receive raw `mcp_search` queries or `mcp_call` arguments.
`ToolApprovalRequired` uses that same safe projection. During provider
streaming, every `ToolCallDelta` argument chunk for either meta-tool is replaced
with `[REDACTED]`, including continuation chunks that omit the tool name;
correlation by tool-call index or ID preserves the redaction decision. Ordinary
tools retain their original approval, start, and streaming-delta payloads. The
registry and MCP dispatcher still receive the original MCP meta-tool values
unchanged. A searched tool that later becomes capability-denied must be denied
at call time. Successful `mcp_call` results follow the complementary boundary:
the exact result is delivered to the model, while registry result logs expose
only `[REDACTED]` plus result length and `ToolOutput` activity/server SSE expose
only `[REDACTED] (result_length=<n>)`. Ordinary-tool result logging and activity
remain unchanged.

## Behavior

### Configuration and bootstrap

**Evidence:** `s057_skill_frontmatter`, CLI
`configured_mcp_bootstrap_exposes_only_stable_meta_tools_without_connecting`,
server provider-injection coverage, and child catalog prevalidation tests cover
parsing, allow-lists/capabilities, lazy descriptors, and the fixed provider
surface.

- [x] `SKILL.md` frontmatter accepts optional `mcp_servers` as an array of
  non-empty strings and exposes the normalized server-name list in skill
  metadata.
- [x] Invalid `mcp_servers` frontmatter (a non-array value, a non-string item,
  or an empty/whitespace-only server name) makes that skill invalid under the
  existing S017 invalid-skill handling; it is never partially activated.
- [x] At bootstrap, every declared server for each otherwise eligible skill is
  validated against configured MCP server descriptors and the agent/tenant MCP
  allow-list that applies to that skill's agent type.
- [x] A skill that references an unknown configured server, a server outside
  the applicable tenant/agent allow-list, or a server denied by MCP capability
  policy fails bootstrap with an actionable error naming the skill and server;
  bootstrap makes no network request for the rejected dependency.
- [x] Bootstrap retains only the authorized MCP descriptors needed for later
  per-agent activation and does not connect, authenticate, or inventory any
  configured MCP server.
- [x] A runtime with configured MCP servers registers exactly one direct
  `mcp_search` tool and exactly one direct `mcp_call` tool for an agent; a
  runtime with no configured MCP servers registers neither.
- [x] The initial provider request contains the stable meta-tool definitions
  (when MCP is configured) but contains no configured or activated MCP tool
  schema, server inventory, credential, endpoint, or MCP tool description.
- [x] The provider-visible schemas for `mcp_search` and `mcp_call` remain byte
  equivalent for the life of an agent session, regardless of skill loads,
  activation success/failure, or MCP inventory changes.
- [x] Existing direct registration of one `ToolDefinition` per configured MCP
  tool is removed from the agent/provider toolset.

### Atomic skill activation

**Evidence:** `McpCatalog` rollback/cache tests plus the production CLI and
server skill-call paths cover prevalidation, one-time inventory, atomic commit,
preservation of earlier state, and body withholding on failure.

- [x] Model-triggered `Skill(command)` and user-triggered `/skill-name`
  resolve the same skill metadata and use the same MCP activation path before
  the skill body is returned or injected.
- [x] Loading a skill without `mcp_servers` preserves S017 loading behavior and
  does not contact an MCP server.
- [x] Before activation makes a network request, Simulacra validates every
  declared server's configuration eligibility and the skill's current
  capability eligibility; any failure returns an actionable skill-load error
  with no dependency connection attempt.
- [x] For each newly declared server, activation performs the existing MCP
  handshake and `tools/list` inventory exactly once before exposing the skill
  body or any schemas from that skill's dependency set.
- [x] Activation is atomic across every newly declared server of one skill:
  if any handshake or inventory fails, the skill body is not returned/injected,
  no newly successful sibling server catalog is committed, and no sibling tool
  schema becomes searchable or callable.
- [x] An activation failure leaves previously activated catalogs and
  search-publications from earlier successful skill loads unchanged.
- [x] After a successful activation, the skill body and every newly activated
  server inventory become visible together to that agent session.
- [x] Re-loading a successfully activated skill, or loading another skill that
  declares an already activated server, reuses the cached inventory and does
  not reconnect, re-handshake, duplicate index entries, or invalidate existing
  search-publications.
- [x] Activation of one declared server never connects, inventories, or reveals
  tools from another configured server that has not been activated for this
  agent.
- [x] A successful activation builds a replacement immutable catalog-and-index
  snapshot before atomically swapping it into the agent session; searches never
  observe a partially indexed inventory.
- [x] A later successful activation replaces the session snapshot with one that
  contains both earlier and newly committed server inventories and index
  entries, without invalidating earlier search-publications.
- [x] A failed activation leaves the prior snapshot byte-for-byte observable
  through search, and repeated activation of a committed server adds no
  duplicate inventory or index entries.

### Catalog search and dispatch

**Evidence:** bounded-publication, dispatch-capability, rollback-preservation,
catalog-isolation, and provider-injection tests exercise the real catalog and
MCP dispatcher paths. Existing S008/S024/S041 tests continue to govern the
manager path used by `mcp_call`. `successful_search_and_call_redact_secret_inputs_from_journal_and_tracing`
and the reconciled MCP protocol tests prove raw search queries and call
arguments still reach their functional paths but are replaced in journals and
telemetry by query/argument length plus non-secret server/tool metadata.
`mcp_meta_tool_outer_journal_redacts_inputs_without_changing_registry_dispatch`
proves the same boundary at the outer runtime journal and `ToolStart` activity
surface; downstream activity and SSE serialization consume that already-safe
payload rather than the provider's raw meta-tool input. HITL and streaming tests
prove `ToolApprovalRequired` uses the metadata-only shape and every MCP
`ToolCallDelta`, including unnamed continuation chunks, emits `[REDACTED]`.
Those tests also prove ordinary-tool arguments and deltas are unchanged.
Successful-result tests prove the exact MCP result reaches the model while
registry logs, `ToolOutput`, and server SSE contain only redaction markers and
result length; ordinary-tool output remains unchanged.

- [x] `mcp_search` returns only tools from the calling agent's successfully
  activated server catalogs, returns at most five results, and includes each
  result's server, tool name, description, and input schema.
- [x] `mcp_search` never connects or inventories an inactive configured server.
- [x] `mcp_search` publishes each returned `(server, tool)` pair for later
  `mcp_call` in the same agent session and does not publish omitted matches
  beyond the five-result bound.
- [x] `mcp_call` succeeds for an activated, search-published tool with valid
  arguments and forwards those arguments unchanged to the existing MCP
  dispatcher.
- [x] `mcp_call` rejects a configured-but-inactive server, an inactive tool, or
  an activated tool not previously returned by `mcp_search`, before an MCP
  network tool call is attempted.
- [x] `mcp_call` checks `mcp:<server>:<tool>` capability at dispatch even when
  the skill activation and search publication previously succeeded.
- [x] A capability-denied `mcp_call` produces the existing actionable MCP
  capability error and does not invoke the remote tool.
- [x] `mcp_call` preserves existing MCP hooks, journal-before-return behavior,
  transport retry behavior, and tool result/error semantics; the meta-tool is
  not a bypass around S008/S024/S041.
- [x] Search publication, activated inventory, and cached server state are
  isolated by agent session: concurrent agents may activate and search the
  same configured server independently, and a publication or activation in one
  agent cannot make a tool callable or discoverable in another.
- [x] Catalog indexing lowercases server names, tool names, descriptions, and
  queries and splits each at non-alphanumeric boundaries.
- [x] Every distinct normalized query term must match somewhere in a returned
  tool's indexed server name, tool name, or description; reordering query terms
  does not change matches, eligibility, base per-term relevance, or tie
  behavior; duplicate query terms do not change the result, and a tool missing
  any term is excluded.
- [x] A query term matches an indexed token exactly or by prefix only when the
  query term contains at least three characters; one- and two-character terms
  do not prefix-match.
- [x] Per-term relevance orders matches by exact tool-name token, tool-name
  prefix, exact server-name token, server-name prefix, exact description token,
  then description prefix.
- [x] A complete normalized query equal to the complete normalized tool name
  receives the strongest relevance boost and ranks ahead of tools that match
  the same terms only through individual tokens or lower-weight fields. This
  ordered complete-name boost may distinguish reordered query-term
  permutations.
- [x] Equal-ranked matches are ordered deterministically by configured server
  name and then tool name; an empty query returns the alphabetically first five
  tools using that same server-then-tool ordering.
- [x] Search selects at most five winners with bounded top-k selection rather
  than sorting every match, and only winning results clone or serialize their
  full input schemas.
- [x] Indexed lookup prunes candidates for a selective non-empty query so the
  number of candidates considered is smaller than the number of activated
  catalog tools while preserving the correct ranked results.
- [x] Search ranks against an immutable session snapshot after releasing
  mutable catalog state, and concurrent agents never share snapshots, index
  entries, candidates, or publications.
- [x] A successful `mcp_search` records its safe journal entry before publishing
  or returning the selected results, preserving the existing publication and
  journal-before-return ordering.

### Capability attenuation and lifecycle

**Evidence:** native-child capability and tenant-isolation tests construct the
real child environment; catalog isolation tests prove publications are owned by
the catalog instance. `search_and_remote_call_errors_are_actionable_without_leaking_backend_secrets`
injects secret-bearing journal and remote JSON-RPC failures and verifies both
returned errors and captured telemetry are redacted.
`legacy_sse_activation_failure_redacts_endpoint_secrets_but_retains_safe_context`
proves legacy SSE URL, userinfo, query, authorization, and module-path details
are removed while server/transport/stage context remains actionable.

- [x] `mcp_servers` does not widen a skill's, parent's, child's, tenant's, or
  agent's effective MCP permissions; it names dependencies that must already
  be configured and allowed.
- [x] A child agent validates and activates skills using its own effective,
  attenuated MCP capability and tenant/agent allow-list; it does not inherit a
  parent agent's activated catalog or search-publications.
- [x] An activated server catalog and its search-publications remain available
  for the rest of that agent session, including later turns, and are discarded
  when the session ends.
- [x] Activation, search, and call error results never disclose credentials,
  authorization headers, or secret descriptor fields.

## Observability and audit

**Evidence:** catalog telemetry tests capture activation outcomes, counts,
server sets, caching, and secret redaction. The production OTLP harness from
`1eb2345` passed against local Aniani: TraceQL found `execute_tool`, PromQL found
`simulacra_mcp_calls{server="github",tool="issues"} = 1`, and LogQL found
activation success/failure plus catalog-search evidence; the same test asserts
activation/search journal attribution. Model and interactive tests additionally
assert explicit source/link fields and span correlation across the user
activation thread bridge. Search telemetry records `query_length`, result count,
and returned server/tool identifiers; call telemetry records server/tool and
`argument_length`. Outer AgentLoop journal entries and `ToolStart` activity
events use the same safe shapes, which are the shapes exposed to activity/SSE
consumers. `ToolApprovalRequired` is projected identically. Streaming
`ToolCallDelta` activity/SSE replaces all MCP meta-tool argument chunks with
`[REDACTED]`, carrying the decision across unnamed continuation chunks by
tool-call index or ID. Ordinary-tool activity remains unchanged. No runtime
audit, telemetry, approval, activity, or SSE layer records the raw MCP query or
raw MCP call arguments. Successful MCP result observability is also bounded:
registry logs retain result length, and `ToolOutput` activity/server SSE retain
only the redaction marker plus result length, while the exact result remains in
the model-facing tool message.

- [x] Every activation attempt emits an activation trace/span or event linked to
  the triggering skill load and records `simulacra.skill.name`, the declared
  server-name set, activated-tool count, and outcome (`success` or `failure`),
  without credentials or endpoint secrets.
- [x] A failed multi-server activation emits one failure outcome for the skill
  and does not report a successful activated catalog for that failed attempt.
- [x] Successful activation records the count of tools newly committed to that
  agent's catalog; a cached repeated activation records zero newly activated
  tools and does not produce a new handshake/inventory span.
- [x] `mcp_search` emits trace/log evidence of query length, result count, and
  only non-secret server/tool identifiers; it does not record the raw query,
  arguments, credentials, or inactive-server inventory.
- [x] `mcp_search` trace/log evidence also records activated catalog-tool count
  and indexed candidate count. A selective query demonstrates a candidate count
  lower than catalog-tool count, while traces, logs, metrics, journals,
  activities, and SSE expose neither raw nor normalized query terms.
- [x] Every dispatched `mcp_call` retains S008 observability: an
  `execute_tool` span, `simulacra.tool.name`, `simulacra.tool.source =
  mcp:<server>`, MCP call metric labels, and `gen_ai.tool.message` input-metadata
  and output events. Input telemetry contains server/tool and argument length,
  never raw arguments; outer `ToolStart` and `ToolApprovalRequired` activity and
  downstream SSE expose the same metadata-only shape. Streaming MCP
  `ToolCallDelta` chunks, including continuation chunks, expose `[REDACTED]`.
  Successful result telemetry contains `[REDACTED]` and result length;
  `ToolOutput` activity and server SSE contain the same safe result metadata.
  Ordinary tools remain unchanged, while MCP dispatch receives the original
  arguments and the model receives the exact MCP result unchanged.
- [x] Every successful remote MCP call retains the existing journal entry before
  its result reaches the agent; local activation/search bookkeeping is recorded
  so the skill dependency and catalog publication remain attributable. Both the
  outer AgentLoop journal and MCP manager audit use safe metadata: search
  entries store query length; call entries store server/tool and argument
  length. Raw query text, raw arguments, and credentials are not recorded at
  either layer.
- [x] Local Aniani validation demonstrates activation success and atomic
  failure traces, `mcp_search` evidence, MCP call spans/metrics/logs, and the
  corresponding journal entries using TraceQL, PromQL, and LogQL.

## Acceptance Test Matrix

The implementation must provide behavioral tests (not source-scanning tests)
covering every unchecked assertion above, including these minimum scenarios:

**Evidence:** the checked scenarios are covered by frontmatter fixtures, CLI
bootstrap probes, server provider-injection tests, MCP rollback/capability
tests, and the local Aniani harness described above. The final matrix tests
activate exactly two declared servers while leaving a configured third dormant,
and use `tokio::join!` to prove two concurrent catalogs remain isolated while
cached reactivation avoids reconnection and duplicate indexing.

- [x] A skill frontmatter fixture recognizes `mcp_servers`; unknown,
  tenant-disallowed, and capability-denied references fail bootstrap or skill
  activation before a fake MCP server observes any network request.
- [x] The first provider request with configured MCP includes only stable
  `mcp_search`/`mcp_call` MCP surfaces and never an MCP server tool schema.
- [x] Loading a skill with two declared fake MCP servers performs handshake and
  inventory for exactly those two; a later search returns only their activated
  tools, never another configured fake server's tools.
- [x] A successful search-published activated tool is callable through
  `mcp_call`, and its call still enforces the exact
  `mcp:<server>:<tool>` capability namespace.
- [x] If one of multiple newly declared servers fails activation, the skill body
  is withheld and neither the successful sibling's schemas nor the failed
  sibling's schemas can be searched or called.
- [x] Two concurrently running agents have separate catalogs; repeated loading
  of one skill/server does not reconnect or duplicate that agent's inventory.
- [x] An Aniani-backed integration test validates activation and MCP-call
  traces, metrics, logs, and journal entries through TraceQL, PromQL, and
  LogQL using a local deterministic MCP fixture.
- [x] Behavioral search tests cover case and boundary normalization, reordered
  and duplicate multi-term queries, rejection when any term is unmatched,
  exact-name precedence, the two-character/three-character prefix boundary,
  deterministic ties, and alphabetical empty-query results.
- [x] A catalog with more than five matching tools proves bounded five-result
  selection and publication: only the returned five become callable.
- [x] Activation tests prove a later atomic snapshot swap adds searchable tools,
  a failed activation preserves the earlier indexed results, and repeated
  activation creates neither duplicate results nor duplicate index entries.
- [x] Concurrent catalog tests prove the immutable index snapshot and search
  publications remain isolated per agent session.
- [x] Captured telemetry and local Aniani evidence prove a selective query has
  candidate count below catalog-tool count and that no raw or normalized query
  term appears in telemetry or audit surfaces.
