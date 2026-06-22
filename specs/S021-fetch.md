# S021 — WHATWG-Aligned Fetch API & HTTP Control Plane

**Status:** Active
**Crates involved:** `simulacra-http`, `simulacra-fetch`, `simulacra-quickjs`, `simulacra-sandbox`

## Dependencies

- **ARCHITECTURE.md** — Golden Rule, single-binary philosophy, capability attenuation
- **S003** — QuickJS runtime: module bindings, host function contracts
- **S004** — Capability tokens: network permission gating
- **S006** — Resource budgets: turns consumption on HTTP calls
- **S010** — Observability conventions: span schemas, metric names
- **S011** — Sandbox: Golden Rule enforcement
- **S016** — Native modules: registration patterns for QuickJS globals

## Scope

Two layers:

1. **`simulacra-http`** — HTTP control plane. Shared, governed HTTP surface that all HTTP paths converge on (JS fetch, shell wget/curl, module fetching, MCP transport). Enterprise-grade security, observability, and audit.
2. **`simulacra-fetch`** — WHATWG Fetch Standard-aligned JS classes. Thin wrappers over simulacra-http.

Full design: `docs/superpowers/specs/2026-03-22-s021-fetch-design.md`

## Behavior

### simulacra-http: HttpClient trait & UreqHttpClient

- [x] `UreqHttpClient::execute` sends a GET request and returns status, headers, and body.
- [x] `UreqHttpClient::execute` sends a POST with body and custom headers.
- [x] `UreqHttpClient::execute` returns `HttpError::Timeout` when the request exceeds `timeout_ms`.
- [x] `UreqHttpClient::execute` returns `HttpError::Network` on connection failure (DNS, TLS, refused).
- [x] `UreqHttpClient::execute` follows redirects and sets `redirected: true` on the response.
- [x] `UreqHttpClient::execute` populates `status_text` from the HTTP response reason phrase.
- [x] `UreqHttpClient::execute` populates `url` with the final URL after redirects.
- [x] Default timeout is 5 seconds when `timeout_ms` is `None`.
- [x] Default max redirects is 5.
- [x] `UreqHttpClient::execute` returns `HttpError::TooManyRedirects` when the redirect count exceeds `max_redirects`.
- [x] `UreqHttpClient::execute` returns `HttpError::InvalidUrl` for malformed URL input.
- [x] Response body size is capped at `MAX_RESPONSE_SIZE` (10 MiB); oversize responses return `HttpError::ResponseTooLarge`.
- [x] Sensitive headers (Authorization, Cookie, etc.) are redacted in log output.
- [x] URLs are sanitized (strip userinfo) before appearing in span attributes.
- [x] `simulacra-sandbox::fetch_http_inner` uses `HttpClient::execute` (direct `ureq` usage replaced).

### simulacra-fetch: Headers class (Rust API + JS binding)

- [x] `new Headers()` creates empty headers.
- [x] `new Headers(existing)` copies from another `Headers` instance.
- [x] `new Headers({"Content-Type": "application/json"})` creates headers from a plain object.
- [x] `new Headers([["x-custom", "value"]])` creates headers from an array of pairs.
- [x] `headers.get("content-type")` is case-insensitive.
- [x] `headers.get` returns comma-joined values for duplicate names.
- [x] `headers.set` replaces all values for the name.
- [x] `headers.append` adds without replacing.
- [x] `headers.has` / `headers.delete` are case-insensitive.
- [x] Iteration (`keys`, `values`, `entries`, `for...of`, `forEach`) yields entries sorted by name.
- [x] Invalid header name throws a `TypeError` at the JS boundary.
- [x] Invalid header value throws a `TypeError` at the JS boundary.
- [x] Header values are trimmed of surrounding whitespace.

### simulacra-fetch: Blob class

- [x] `new Blob(["hello"])` creates a blob with size 5 and empty type.
- [x] `new Blob(["hello"], { type: "text/plain" })` sets the type.
- [x] `new Blob([blob1, "extra"])` concatenates parts.
- [x] `new Blob([arrayBuffer, typedArray])` accepts ArrayBuffer and TypedArray parts.
- [x] `blob.text()` returns a UTF-8 string of the bytes.
- [x] `blob.arrayBuffer()` returns an ArrayBuffer copy.
- [x] `blob.bytes()` returns a `Uint8Array` copy.
- [x] `blob.slice(start, end, type?)` returns a sub-blob with the requested range.
- [x] `blob.slice` clamps out-of-range indices and returns an empty blob when start > end.
- [x] `blob.slice` accepts negative indices (offset-from-end).
- [x] TypedArray view respects `byteOffset` / `byteLength` when used as a Blob part.
- [x] `Blob` must be invoked with `new` (non-constructor call throws).

### simulacra-fetch: Request class

- [x] `new Request("https://example.com")` creates a GET request with empty headers.
- [x] `new Request(url, { method: "POST", body: "data" })` sets method and body.
- [x] `new Request(existing)` clones an existing request.
- [x] `new Request(existing, { method: "PUT" })` overrides cloned fields.
- [x] `request.headers` returns a `Headers` instance.
- [x] Body consumption methods (`text`, `json`, `arrayBuffer`, `bytes`, `blob`) return Promises.
- [x] Second body consumption throws `TypeError`.
- [x] `request.clone()` produces an independent copy.
- [x] `request.clone()` after body consumption throws `TypeError`.
- [x] Blob body auto-sets `Content-Type` from the blob's type.
- [x] GET/HEAD requests reject non-null bodies with `TypeError`.

### simulacra-fetch: Response class

- [x] `new Response("body", { status: 201 })` creates a custom response.
- [x] `Response.json({ key: "value" })` creates a JSON response with `Content-Type: application/json`.
- [x] `Response.error()` returns a Response with status 0 and type `"error"`.
- [x] `Response.redirect("https://x", 301)` sets the `Location` header.
- [x] `Response.redirect(url, 200)` throws `RangeError` (only 301/302/303/307/308 allowed).
- [x] `response.ok` is `true` for status 200–299, `false` otherwise.
- [x] `response.headers` returns a `Headers` instance populated from response headers.
- [x] Body consumption methods enforce single-consumption.
- [x] `response.clone()` produces an independent copy.
- [x] `response.clone()` after body consumption throws `TypeError`.
- [x] `response.body` is `null` (no `ReadableStream`).
- [x] `response.blob()` returns a Blob with `Content-Type` from headers.
- [x] Invalid status outside 200–599 throws `RangeError`.

### simulacra-fetch: AbortController / AbortSignal

- [x] `new AbortController()` creates a controller with a non-aborted signal.
- [x] `controller.abort()` sets `signal.aborted = true` with the default `AbortError` reason.
- [x] `controller.abort(reason)` stores the custom reason.
- [x] `signal.throwIfAborted()` throws when aborted, is a no-op otherwise.
- [x] `AbortSignal.abort()` returns a pre-aborted signal.
- [x] `AbortSignal.timeout(5000)` returns a non-aborted signal that carries timeout metadata.
- [x] `fetch(url, { signal: abortedSignal })` rejects immediately with the signal's reason.

### simulacra-fetch: fetch() function

- [x] `fetch("https://example.com")` returns a `Promise<Response>`.
- [x] `fetch(url, { method, headers, body })` sends the configured request.
- [x] `fetch(new Request(url, init))` accepts a Request as input.
- [x] `fetch(request, { method: "POST" })` lets `init` override a Request-input's fields.
- [x] `fetch(url, { signal: AbortSignal.timeout(100) })` passes the timeout to the proxy.
- [x] Fetch with a Blob body forwards the bytes.
- [x] Capability-denied fetch rejects with a capability error message.
- [x] Budget-exhausted fetch rejects with a budget error message (distinct from capability).
- [x] Network error rejects with a `TypeError` (per WHATWG).
- [x] Timeout rejects with `{ name: "TimeoutError" }`.
- [x] Successful fetch produces a Response with correct status, statusText, headers, url, redirected.

### FetchProxy bridge

- [x] `FetchProxy` trait is defined in `simulacra-fetch`; implementations live in downstream crates.
- [x] `AgentCellFetchProxy` in `simulacra-sandbox` implements `FetchProxy` by delegating to `AgentCell::fetch_http`.
- [x] `SandboxError::CapabilityDenied` maps to `FetchError::CapabilityDenied`.
- [x] `SandboxError::BudgetExhausted` maps to `FetchError::BudgetExhausted`.
- [x] `SandboxError::Http` maps to `FetchError::NetworkError`.
- [x] `register_globals(ctx, proxy)` installs all 7 globals (fetch, Headers, Request, Response, Blob, AbortController, AbortSignal).
- [x] The old inline `__simulacra_fetch_impl__` host function is removed from `simulacra-quickjs`.
- [x] `FetchProxy` / `FetchResponse` types are no longer defined in `simulacra-quickjs` (moved to `simulacra-fetch`).

## Observability (see S010)

### Internal (platform operator)

- [x] Every HTTP call produces a `simulacra_http_request` span with `http.request.method`, `url.full`, `http.response.status_code`, `http.request.body.size`, `http.response.body.size`.
- [x] HTTP duration is recorded on the span (`simulacra.http.client.duration` histogram semantics).
- [x] HTTP errors are logged at WARN level with URL, method, and error type.
- [ ] Per-agent HTTP call counts and byte totals are queryable via span attributes (`simulacra.agent.id`).

### External (agent developer)

- [x] `fetch()` calls flow through the existing `sandbox_http_fetch` span in `AgentCell::fetch_http`.
- [x] `AbortSignal.timeout()` timeouts surface as `FetchError::Timeout` in logs.

### Audit

- [x] Every HTTP call produces a journal entry via the Golden Rule chain (URL, method, status, agent ID recorded).

## Known divergences from WHATWG

- `AbortSignal.timeout(ms)` does not auto-abort after `ms`. The timeout is metadata passed to the HTTP client for per-request timeout enforcement. `signal.aborted` remains `false` even after the timeout fires — the HTTP call rejects at the Rust layer. Deliberate trade-off for the sync bridge.
- `ReadableStream`, `FormData`, `URLSearchParams` are out of scope.
- `redirect: "error" | "manual"` modes are unsupported — only `"follow"` is implemented.
- `AbortSignal.any()` and `AbortSignal` event listeners are unsupported.
