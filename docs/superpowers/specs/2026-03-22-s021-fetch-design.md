# S021 — WHATWG-Aligned Fetch API & HTTP Control Plane

**Status:** Active
**Crates involved:** `simulacra-http` (new), `simulacra-fetch` (new), `simulacra-quickjs`, `simulacra-sandbox`

## Dependencies

- **ARCHITECTURE.md** — Golden Rule, single-binary philosophy, capability attenuation
- **S003** — QuickJS runtime: module bindings, host function contracts
- **S004** — Capability tokens: network permission gating
- **S006** — Resource budgets: turns consumption on HTTP calls
- **S010** — Observability conventions: span schemas, metric names
- **S011** — Sandbox: Golden Rule enforcement (span → capability → budget → execute → journal → return)
- **S016** — Native modules: registration patterns for QuickJS globals

## Scope

This spec covers two layers:

1. **`simulacra-http`** — The HTTP control plane. A shared, governed HTTP surface that all HTTP paths converge on: JS `fetch()`, shell `wget`/`curl`, module fetching, MCP transport, and any future HTTP consumer. Enterprise-grade security, observability, and audit are built in at this layer.

2. **`simulacra-fetch`** — WHATWG Fetch Standard-aligned JS classes (`fetch()`, `Headers`, `Request`, `Response`, `Blob`, `AbortController`/`AbortSignal`) that present a familiar API to agent-authored JS. These are thin wrappers over `simulacra-http`.

**In scope:**
- `simulacra-http`: HTTP client with capability gating, budget enforcement, per-request tracing, request/response journaling, timeout management, connection pooling (future-ready)
- `simulacra-fetch`: `fetch()` global, `Headers`, `Request`, `Response`, `Blob`, `AbortController`/`AbortSignal`
- `AgentCellFetchProxy` wiring in simulacra-sandbox
- Shell `wget`/`curl` routing through `simulacra-http` (design only — implementation may be a follow-up)

**Out of scope:**
- `ReadableStream` / streaming bodies — bodies are fully buffered (sync bridge)
- `FormData`, `URLSearchParams` — agents use `JSON.stringify()` for request bodies
- CORS, `mode`, `credentials`, `cache`, `referrer`, `integrity` — browser security model replaced by capability tokens (S004)
- `redirect` modes `"error"` and `"manual"` — only `"follow"` supported
- `AbortSignal.any()` — minimal agent value
- Event listeners on `AbortSignal` — no DOM event system in QuickJS
- Connection pooling, circuit breaking, rate limiting — future hardening on top of this foundation

**Known WHATWG divergences:**
- `AbortSignal.timeout(ms)` does not auto-abort the signal after `ms` milliseconds. Instead, the timeout is passed as metadata to the HTTP client for per-request timeout enforcement. This is because the sync bridge makes async timer-based abort impossible. Agent code checking `signal.aborted` after a timeout will see `false` — the signal never becomes aborted, the HTTP call simply times out at the Rust layer. This is a deliberate trade-off for simplicity.

**Cross-spec updates required:**
- S003 behaviors 13-16 (current minimal fetch) are superseded by this spec. S003 should be updated to reference S021 for fetch behavior.
- S011 behavior 19 references `reqwest` but the codebase uses `ureq`. Should be corrected (separate task).

## Context

### Why a dedicated HTTP control plane

Every HTTP request an agent makes is a security, cost, and audit event. Today, HTTP is scattered:
- JS `fetch()` has a toy implementation in simulacra-quickjs (plain object, no headers, no abort)
- `AgentCell::fetch_http()` in simulacra-sandbox enforces the Golden Rule but is tightly coupled to the sandbox
- Shell commands that touch the network bypass HTTP governance entirely
- Module fetching has its own HTTP path in `module_fetcher.rs`

For an enterprise agent runtime, HTTP must be a single governed surface — the same way a corporate proxy governs all employee HTTP traffic. `simulacra-http` is that surface.

### Enterprise requirements this enables

- **Security:** Domain allowlists, request sanitization, capability enforcement — one policy engine for all HTTP
- **Observability (internal):** Platform operators see every HTTP call: latency histograms, error rates, bandwidth, per-agent breakdown
- **Observability (external):** Agent developers see their fetch calls in traces with timing, status, headers
- **Audit:** Full request/response journal entries for compliance, replay, forensic analysis
- **Economics:** Every HTTP call is metered against the agent's budget (turns, cost, bandwidth)
- **Risk:** Anomaly detection surface — unusual domains, high request rates, large payloads can trigger alerts
- **Deployment-agnostic:** Works identically in SaaS and managed in-cloud deployments

### Current `fetch()` limitations

The current `fetch()` in simulacra-quickjs is ~40 lines of JS returning a plain object with `.status`, `.text()`, `.json()`. No `Headers` class, no `Request` class, response headers discarded, no `Blob`, no `AbortController`. Agent JS that uses standard Fetch API patterns fails silently.

## Design

### Architecture

```text
Agent JS code               Shell commands
    │                           │
    ▼                           ▼
fetch(url, init)            wget / curl
    │                           │
    ├─ Headers class            │
    ├─ Request class            │
    ├─ Response class           │
    ├─ Blob class               │
    ├─ AbortController          │
    │                           │
    ▼                           ▼
simulacra-fetch                 simulacra-shell
FetchProxy trait            ShellExecutor
    │                           │
    └──────────┬────────────────┘
               ▼
         simulacra-http
         HttpClient trait
               │
               ├─ Capability check (S004)
               ├─ Budget enforcement (S006)
               ├─ Span creation (S010)
               ├─ Request/response journaling (S005)
               ├─ Timeout management
               ├─ Connection management
               │
               ▼
         ureq (HTTP/TLS)
```

### Crate structure

```
crates/simulacra-http/                    # HTTP control plane
├── Cargo.toml                        # depends on ureq, simulacra-types, tracing
├── src/
│   ├── lib.rs                        # HttpClient trait, re-exports
│   ├── client.rs                     # HttpClient implementation (ureq-backed)
│   ├── types.rs                      # HttpRequest, HttpResponse, HttpError
│   └── policy.rs                     # Request policy hooks (future: rate limits, circuit breakers)

crates/simulacra-fetch/                   # WHATWG Fetch JS classes
├── Cargo.toml                        # depends on rquickjs, simulacra-http, serde, serde_json, thiserror
├── src/
│   ├── lib.rs                        # register_globals(), re-exports
│   ├── headers.rs                    # Headers class (rquickjs #[class])
│   ├── request.rs                    # Request class
│   ├── response.rs                   # Response class
│   ├── blob.rs                       # Blob class
│   ├── abort.rs                      # AbortController + AbortSignal
│   └── fetch.rs                      # fetch() global, FetchProxy trait + bridge
```

### Dependency graph

```text
simulacra-http (HTTP control plane — trait + client + types)
    ↑
simulacra-fetch (WHATWG JS classes, depends on simulacra-http for types)
    ↑
simulacra-quickjs (calls register_globals during runtime init)
    ↑
simulacra-sandbox (implements FetchProxy via AgentCell, uses simulacra-http::HttpClient)
```

### simulacra-http separation from simulacra-sandbox

Today `AgentCell::fetch_http()` and `fetch_http_inner()` in simulacra-sandbox own the HTTP call, Golden Rule enforcement, and `ureq` integration. With `simulacra-http`:

- **`simulacra-http`** owns: the HTTP client (`ureq`), request/response types, timeout management, connection config. It exposes an `HttpClient` trait and a `UreqHttpClient` implementation.
- **`simulacra-sandbox`** owns: Golden Rule orchestration (capability → budget → execute → journal). It calls `simulacra-http::HttpClient::execute()` for the actual HTTP call, replacing direct `ureq` usage.
- **`simulacra-fetch`** owns: JS class bindings only. Defines `FetchProxy` trait. `AgentCellFetchProxy` in simulacra-sandbox implements it.

This means `simulacra-http` is a pure HTTP client with no agent/sandbox concepts. The agent-specific governance (capabilities, budgets, journals) stays in simulacra-sandbox where it belongs. `simulacra-http` provides the knobs (timeouts, headers, connection config) that the sandbox layer controls.

## Behavior

### simulacra-http: HttpClient

1. `HttpClient` trait defines the low-level HTTP execution surface:
   ```rust
   pub trait HttpClient: Send + Sync {
       fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError>;
   }
   ```
2. `HttpRequest` contains: `url: String`, `method: String`, `headers: Vec<(String, String)>`, `body: Option<Vec<u8>>`, `timeout_ms: Option<u64>`, `max_redirects: Option<u32>`.
3. `HttpResponse` contains: `status: u16`, `status_text: String`, `headers: Vec<(String, String)>`, `body: Vec<u8>`, `url: String` (final URL after redirects), `redirected: bool`.
4. `HttpError` enum: `Network(String)` (connection failed, DNS resolution, TLS), `Timeout` (request exceeded timeout), `TooManyRedirects`, `InvalidUrl(String)`.
5. `UreqHttpClient` implements `HttpClient` using `ureq`. Configurable default timeout (5 seconds), max redirects (5), and user-agent string.
6. `simulacra-sandbox/src/http.rs` is refactored: `do_http_request()` is deleted and replaced by `HttpClient::execute()`. `fetch_http_inner()` calls `HttpClient::execute()` instead of `do_http_request()`. The Golden Rule enforcement (span, capability, budget, journal) remains in simulacra-sandbox.

### simulacra-fetch: Headers class

7. `Headers` stores entries as `Vec<(String, String)>`. Header names are lowercased on insertion.
8. Constructor accepts: no arguments (empty), a `Headers` instance (copy), a plain object `{key: value}`, or an array `[["key", "value"]]`.
9. `get(name)` returns comma-joined values for all entries matching the lowercased name, or `null` if none.
10. `set(name, value)` removes all existing entries for the name, then adds one entry.
11. `has(name)` returns `true` if any entry matches the lowercased name.
12. `delete(name)` removes all entries matching the lowercased name.
13. `append(name, value)` adds a new entry without removing existing ones for that name.
14. `forEach(callback)` iterates entries sorted by name (byte-level sort, per WHATWG "sort and combine" algorithm — equivalent to lexicographic since names are lowercased).
15. `keys()`, `values()`, `entries()` return iterators over entries sorted by name.
16. `Headers` is iterable with `for...of`, yielding `[name, value]` pairs.
17. No `HeadersGuard` — all headers are mutable (CORS enforcement not applicable).

### simulacra-fetch: Blob class

18. `Blob` wraps `Vec<u8>` (body bytes) and a MIME type string.
19. Constructor: `new Blob(parts?, options?)`. `parts` is an array of `string | Blob | ArrayBuffer | TypedArray`. `options` is `{ type?: string }`. Parts are concatenated in order.
20. `size` property returns byte length (read-only).
21. `type` property returns the MIME type string, lowercased (read-only). Empty string if not provided.
22. `text()` returns `Promise<string>` — UTF-8 decode of the bytes.
23. `arrayBuffer()` returns `Promise<ArrayBuffer>` — copy of the bytes.
24. `bytes()` returns `Promise<Uint8Array>` — copy of the bytes.
25. `slice(start?, end?, contentType?)` returns a new `Blob` containing the specified byte range with an optional new content type.
26. No `stream()` method — `ReadableStream` is out of scope.

### simulacra-fetch: Request class

27. Constructor: `new Request(input, init?)`. `input` is a URL string or a `Request` instance (cloned). `init` fields override the cloned request's values.
28. `init` supports: `method` (string), `headers` (HeadersInit), `body` (string | Blob | ArrayBuffer | TypedArray | null), `signal` (AbortSignal).
29. Read-only properties: `url` (string), `method` (string, uppercased), `headers` (Headers), `signal` (AbortSignal or null), `bodyUsed` (boolean).
30. Body consumption methods: `text()`, `json()`, `arrayBuffer()`, `bytes()`, `blob()` — all return Promises, all enforce single-consumption. Second call throws `TypeError: body already consumed`.
31. `clone()` returns a deep copy. Throws `TypeError` if body is already consumed.
32. When body is a `Blob`, `Content-Type` is auto-set from the blob's `type` (if not already in headers). When body is a string, sent as UTF-8 bytes. When body is `ArrayBuffer`/`TypedArray`, sent as raw bytes.
33. No `redirect`, `mode`, `credentials`, `cache`, `referrer`, `integrity` properties.

### simulacra-fetch: Response class

34. Constructor: `new Response(body?, init?)`. `body` is `string | Blob | ArrayBuffer | TypedArray | null`. `init` is `{ status?: number, statusText?: string, headers?: HeadersInit }`.
35. Static method `Response.json(data, init?)` creates a Response with `JSON.stringify(data)` as body and `Content-Type: application/json`.
36. Static method `Response.error()` returns a Response with type `"error"`, status `0`, empty body.
37. Static method `Response.redirect(url, status?)` returns a Response with `Location` header set and status (default 302). Throws `RangeError` if status is not 301, 302, 303, 307, or 308.
38. Read-only properties: `status` (number), `statusText` (string), `ok` (true if status 200-299), `headers` (Headers), `url` (string — final URL after redirects), `redirected` (boolean), `type` (`"basic"` or `"error"`), `body` (null — no ReadableStream), `bodyUsed` (boolean).
39. Body consumption methods: `text()`, `json()`, `arrayBuffer()`, `bytes()`, `blob()` — all return Promises, single-consumption enforced. `blob()` constructs a `Blob` from the response bytes with `Content-Type` from headers.
40. `clone()` returns a deep copy (byte clone — bodies are fully buffered). Throws `TypeError` if body already consumed.

### simulacra-fetch: AbortController / AbortSignal

41. `new AbortController()` creates a controller with a fresh `AbortSignal` on its `signal` property.
42. `controller.abort(reason?)` marks the signal as aborted. Default reason: `{ name: "AbortError", message: "The operation was aborted" }`.
43. `signal.aborted` (boolean) — whether the signal has been aborted.
44. `signal.reason` — the abort reason, or `undefined` if not aborted.
45. `signal.throwIfAborted()` — throws `signal.reason` if aborted, otherwise no-op.
46. `AbortSignal.abort(reason?)` — static method returning a pre-aborted signal.
47. `AbortSignal.timeout(ms)` — static method returning a signal that carries a timeout duration. The signal is not pre-aborted; the timeout is passed to the HTTP client via `FetchProxy`.
48. No `AbortSignal.any()`, no `onabort` event handler, no `addEventListener`.

### simulacra-fetch: fetch() function and FetchProxy bridge

49. `fetch(input, init?)` where `input` is a URL string or `Request` instance. Returns `Promise<Response>`.
50. When `input` is a `Request`, its url/method/headers/body/signal are used as defaults. `init` fields override them.
51. Before making the HTTP call, if the signal is aborted, the Promise rejects immediately with the signal's reason.
52. If the signal carries a timeout (from `AbortSignal.timeout(ms)`), the timeout duration is passed to `FetchProxy::fetch()` as `timeout_ms`.
53. On success, constructs a `Response` from the `FetchResponse` returned by the proxy.
54. On `FetchError::CapabilityDenied`, rejects with a capability error message.
55. On `FetchError::BudgetExhausted`, rejects with a budget exhaustion error message (distinct from capability denial).
56. On `FetchError::NetworkError`, rejects with a `TypeError` (per WHATWG — network errors reject, not return error responses).
57. On `FetchError::Timeout`, rejects with `{ name: "TimeoutError", message: "The operation timed out" }`.
58. Mid-flight abort is not possible (sync bridge). `FetchError::Aborted` exists for future async support but is not triggered in this implementation.

### FetchProxy trait

59. `FetchProxy` trait bridges `simulacra-fetch` JS classes to the sandbox layer:
    ```rust
    pub trait FetchProxy: Send + Sync {
        fn fetch(
            &self,
            url: &str,
            method: &str,
            headers: &[(String, String)],
            body: Option<&[u8]>,
            timeout_ms: Option<u64>,
        ) -> Result<FetchResponse, FetchError>;
    }
    ```
60. `FetchResponse` mirrors `simulacra-http::HttpResponse`: `status: u16`, `status_text: String`, `headers: Vec<(String, String)>`, `body: Vec<u8>`, `url: String`, `redirected: bool`.
61. `FetchError` enum: `CapabilityDenied(String)`, `BudgetExhausted(String)`, `NetworkError(String)`, `Timeout`, `Aborted(String)`.
62. `FetchProxy` is defined in `simulacra-fetch`. Implementations live in downstream crates.

### AgentCellFetchProxy (simulacra-sandbox)

63. `AgentCellFetchProxy` implements `simulacra-fetch::FetchProxy` by delegating to `AgentCell::fetch_http()`.
64. `AgentCell::fetch_http()` is updated to accept `timeout_ms: Option<u64>` and pass it through to `simulacra-http::HttpClient::execute()` via `HttpRequest.timeout_ms`.
65. Error mapping: `SandboxError::CapabilityDenied` → `FetchError::CapabilityDenied`, `SandboxError::BudgetExhausted` → `FetchError::BudgetExhausted`, `SandboxError::Http` → `FetchError::NetworkError`.
66. `fetch_http_inner()` is refactored to use `simulacra-http::HttpClient` instead of calling `ureq` directly. The Golden Rule chain (span → capability → budget → execute → journal → return) remains in simulacra-sandbox.

### simulacra-quickjs integration

67. `JsRuntime` calls `simulacra_fetch::register_globals(ctx, proxy)` during initialization when a `FetchProxy` is provided.
68. The old inline `__simulacra_fetch_impl__` host function and `fetch()` JS eval wrapper are deleted from `simulacra-quickjs`.
69. `FetchProxy` trait and `FetchResponse` struct are deleted from `simulacra-quickjs` (moved to `simulacra-fetch`).
70. Existing simulacra-quickjs fetch tests are migrated to use `simulacra-fetch::FetchProxy`.

### ARCHITECTURE.md update

71. The crate dependency graph in ARCHITECTURE.md is updated to include `simulacra-http` and `simulacra-fetch`. Placement: `simulacra-http` is a leaf crate at the same level as `simulacra-types` (no agent/sandbox dependencies). `simulacra-fetch` depends on `simulacra-http` and `rquickjs`, sits between `simulacra-quickjs` and `simulacra-http`. `simulacra-sandbox` depends on `simulacra-http` for the HTTP client.

## Assertions

### simulacra-http: HttpClient

- [ ] `UreqHttpClient::execute()` sends GET request and returns status, headers, body.
- [ ] `UreqHttpClient::execute()` sends POST with body and custom headers.
- [ ] `UreqHttpClient::execute()` returns `HttpError::Timeout` when request exceeds `timeout_ms`.
- [ ] `UreqHttpClient::execute()` returns `HttpError::Network` on connection failure.
- [ ] `UreqHttpClient::execute()` follows redirects and sets `redirected: true` on response.
- [ ] `UreqHttpClient::execute()` populates `status_text` from HTTP response.
- [ ] `UreqHttpClient::execute()` populates `url` with final URL after redirects.
- [ ] `HttpRequest` default timeout is 5 seconds when `timeout_ms` is `None`.
- [ ] `UreqHttpClient::execute()` returns `HttpError::TooManyRedirects` when redirect count exceeds `max_redirects`.
- [ ] `UreqHttpClient::execute()` returns `HttpError::InvalidUrl` for malformed URL input.
- [ ] `simulacra-sandbox::fetch_http_inner` uses `HttpClient::execute()` instead of direct `ureq` calls.
- [ ] `do_http_request()` is deleted from `simulacra-sandbox/src/http.rs`.

### simulacra-fetch: Headers

- [ ] `new Headers()` creates empty headers.
- [ ] `new Headers({"Content-Type": "application/json"})` creates headers from object.
- [ ] `new Headers([["x-custom", "value"]])` creates headers from array.
- [ ] `headers.get("content-type")` is case-insensitive.
- [ ] `headers.set()` replaces all values for the name.
- [ ] `headers.append()` adds without replacing.
- [ ] `headers.has()` and `headers.delete()` work case-insensitively.
- [ ] `headers.get()` returns comma-joined values for duplicate names.
- [ ] Iteration (`keys`, `values`, `entries`, `for...of`, `forEach`) yields entries sorted by name.

### simulacra-fetch: Blob

- [ ] `new Blob(["hello"])` creates blob with size 5 and empty type.
- [ ] `new Blob(["hello"], { type: "text/plain" })` sets the type.
- [ ] `new Blob([blob1, "extra"])` concatenates parts.
- [ ] `blob.text()` returns UTF-8 string.
- [ ] `blob.arrayBuffer()` returns ArrayBuffer with correct bytes.
- [ ] `blob.bytes()` returns Uint8Array.
- [ ] `blob.slice(1, 3)` returns sub-blob.
- [ ] Blob parts accept ArrayBuffer and TypedArray.

### simulacra-fetch: Request

- [ ] `new Request("https://example.com")` creates GET request with empty headers.
- [ ] `new Request(url, { method: "POST", body: "data" })` sets method and body.
- [ ] `new Request(existingRequest)` clones the request.
- [ ] `new Request(existingRequest, { method: "PUT" })` overrides cloned request's method.
- [ ] `request.headers` returns a `Headers` instance.
- [ ] Body consumption methods (`text`, `json`, `arrayBuffer`, `bytes`, `blob`) return Promises.
- [ ] Second body consumption throws `TypeError`.
- [ ] `request.clone()` produces independent copy.
- [ ] `request.clone()` after body consumption throws `TypeError`.
- [ ] Blob body auto-sets `Content-Type` from blob type.

### simulacra-fetch: Response

- [ ] `new Response("body", { status: 201 })` creates custom response.
- [ ] `Response.json({ key: "value" })` creates JSON response with correct headers.
- [ ] `Response.error()` returns response with status 0 and type "error".
- [ ] `Response.redirect("https://example.com", 301)` sets Location header.
- [ ] `Response.redirect(url, 200)` throws RangeError.
- [ ] `response.ok` is true for 200-299, false otherwise.
- [ ] `response.headers` returns a `Headers` instance populated from response headers.
- [ ] Body consumption methods work and enforce single-consumption.
- [ ] `response.clone()` produces independent copy.
- [ ] `response.clone()` after body consumption throws `TypeError`.
- [ ] `response.body` is `null`.
- [ ] `response.blob()` returns Blob with Content-Type from headers.

### simulacra-fetch: AbortController / AbortSignal

- [ ] `new AbortController()` creates controller with non-aborted signal.
- [ ] `controller.abort()` sets `signal.aborted` to true with default reason.
- [ ] `controller.abort(reason)` sets `signal.reason` to the provided reason.
- [ ] `signal.throwIfAborted()` throws when aborted, no-op otherwise.
- [ ] `AbortSignal.abort()` returns pre-aborted signal.
- [ ] `AbortSignal.timeout(5000)` returns signal with timeout metadata.
- [ ] `fetch(url, { signal: abortedSignal })` rejects immediately with abort reason.

### simulacra-fetch: fetch() function

- [ ] `fetch("https://example.com")` returns Promise resolving to Response.
- [ ] `fetch(url, { method: "POST", headers: {...}, body: ... })` sends POST with headers and body.
- [ ] `fetch(new Request(url, init))` accepts Request as input.
- [ ] `fetch(new Request(url, { method: "GET" }), { method: "POST" })` sends POST (init overrides Request).
- [ ] `fetch(url, { signal: AbortSignal.timeout(100) })` passes timeout to proxy.
- [ ] Capability-denied fetch rejects with capability error message.
- [ ] Budget-exhausted fetch rejects with budget error message (distinct from capability).
- [ ] Network error rejects with TypeError.
- [ ] Timeout rejects with TimeoutError.
- [ ] Response includes correct status, statusText, headers, url, redirected.

### FetchProxy bridge

- [ ] `AgentCellFetchProxy` delegates to `AgentCell::fetch_http()`.
- [ ] Capability denials map to `FetchError::CapabilityDenied`.
- [ ] Budget exhaustion maps to `FetchError::BudgetExhausted`.
- [ ] Network errors map to `FetchError::NetworkError`.
- [ ] Timeout maps to `FetchError::Timeout`.
- [ ] `register_globals()` registers all 7 globals (fetch, Headers, Request, Response, Blob, AbortController, AbortSignal).
- [ ] Old `__simulacra_fetch_impl__` and JS wrapper are removed from simulacra-quickjs.
- [ ] Existing simulacra-quickjs fetch tests pass against new implementation.

## Observability (see S010 for conventions)

### Internal (platform operator)

- [ ] Every HTTP call produces a `simulacra_http_request` span with: `http.request.method`, `url.full`, `http.response.status_code`, `http.request.body.size`, `http.response.body.size`.
- [ ] HTTP latency is recorded on a `simulacra.http.client.duration` histogram.
- [ ] HTTP errors are logged at WARN level with URL, method, and error type.
- [ ] Per-agent HTTP call counts and byte totals are available via span attributes (`simulacra.agent.id`).

### External (agent developer)

- [ ] `fetch()` calls flow through the existing `sandbox_http_fetch` span in `AgentCell::fetch_http()`. Agent developers see their fetch calls in traces with timing and status.
- [ ] `AbortSignal.timeout()` timeouts are visible as `FetchError::Timeout` in error logs.

### Audit

- [ ] Every HTTP call produces a journal entry via the existing Golden Rule chain in simulacra-sandbox. Request URL, method, status, and agent ID are recorded.
