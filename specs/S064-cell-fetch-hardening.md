# S064 — Cell fetch hardening: per-hop redirect authorization, https-only, prefetch walk deadline

**Status:** Active — implemented
**Crates involved:** `simulacra-http`, `simulacra-sandbox`, `simulacra-quickjs`
**Downstream driver:** devforge S115 (`js_exec` CDN module imports) — that embedding is
about to grant `net:` host permissions to sandbox cells; this spec closes the two gaps
that would make those grants unsound.

## Why this spec exists

The cell fetch path (`AgentCell::fetch_http` and the module fetcher, both served by
`fetch_http_inner`) authorizes only the INITIAL request host. The concrete
`UreqHttpClient` follows up to five redirects internally with no capability check on any
hop, so an allowed host answering a 302 to anywhere — another origin, an internal
address, a cleartext `http://` endpoint — escapes the granted boundary entirely. The
`check_network` grant is also host-only: it admits any scheme and any port, so a plain
`http://` URL to a granted host is a cleartext/DNS-rebinding hole even without
redirects.

Separately, the static-import prefetch walk
(`prefetch_remote_static_imports_owned`) runs as a `spawn_blocking` task whose JOIN is
timed out; a timed-out walk is abandoned, not stopped, and keeps fetching with no total
deadline. Each fetch is individually bounded (client timeout, body cap, capability
check, visited-set), but the walk itself is unbounded in total.

## Contract

1. **No client-level redirect following on the cell fetch path.** Requests built by
   `fetch_http_inner` ask for redirect following OFF (`max_redirects: Some(0)`), and the
   concrete client surfaces a 3xx as a normal response in that mode (verified: the
   locked ureq already returns the 3xx response at limit zero).
2. **Credentials never ride a redirect.** When the loop follows a redirect, the
   credential headers (`Authorization`, `Proxy-Authorization`, `Cookie`) are stripped
   from the forwarded request — a grant to reach host B never authorizes forwarding
   host A's credentials to it. This restores the previous ureq default
   (`RedirectAuthHeaders::Never`), which the manual loop must not regress.
3. **https-only off the loopback.** The initial URL and every redirect
   destination must use the `https` scheme — EXCEPT loopback destinations
   (`127.0.0.0/8`, `::1`, `localhost`), which may use `http`: cleartext to
   loopback never leaves the machine, and the crate's own localhost-fixture
   integration tests (R004's stated exception) depend on it. Anything else is
   rejected before any request is made. The authorization check and the
   request the transport receives use the SAME parsed, serialized URL — the
   checked host must be the connected host.
4. **Per-hop authorization.** Every hop's host — initial and each redirect destination —
   passes `CapabilityToken::check_network` BEFORE the request for that hop is issued. A
   denied destination fails with the capability denial and results in ZERO requests to
   that destination.
5. **Bounded hops, GET/HEAD only.** At most 5 redirects are followed; the sixth
   hop is rejected. Only GET and HEAD auto-follow — a 3xx answer to any other
   method is a terminal response returned to the caller (bodies are never
   re-sent to a redirect destination).
6. **Walk deadline, per fetch.** The prefetch walk checks its deadline before EVERY
   fetch — a wide module whose source imports many siblings cannot keep fetching past
   the deadline the way a deep chain cannot — and reports the timeout. The
   permit/posture around it is already correct (the walk runs inside the permit-holding
   evaluation worker); this change bounds the walk itself.

Module fetches and `fetch()` ride the same path, so both get all six rules for
coordinator cells and child cells alike. (A pre-registered module stub
short-circuits the HTTP request but still passes the capability and budget
guards — stubs are host-registered test infrastructure, and the scheme gate
does not apply to them.) Journaling, budget checks, spans, and the
existing sanitized-origin telemetry are unchanged — one journal entry per hop is
acceptable and honest (each hop is a real request). A response that followed one or
more redirects reports that it did (the terminal hop's response alone would claim it
did not).

## Acceptance criteria

Driven with a fake `HttpClient` (R004) that records every request and answers scripted
responses — no live network:

- [x] A granted-host `https://` fetch with no redirect succeeds (existing behavior,
      re-pinned).
- [x] Credentials do not ride a redirect: an `Authorization` (or `Cookie`,
      `Proxy-Authorization`) header on the initial request is ABSENT from the
      recorded follow-on request after a redirect.
- [x] A cleartext `http://` initial URL to a LOOPBACK host with a loopback network
      grant succeeds, as does a redirect to one; non-loopback variants stay rejected.
- [x] An initial `http://` URL to a granted host is rejected before any request (zero
      recorded requests).
- [x] A 302 whose `Location` host is not granted: exactly one request (the initial), the
      denial is the capability denial, no request to the destination.
- [x] A 302 whose `Location` is `http://` (even a granted host): denied, no destination
      request.
- [x] A redirect chain that stays inside `https` + granted hosts resolves and returns
      the final body (multiple hops).
- [x] The sixth redirect hop is rejected.
- [x] A relative `Location` resolves against the current URL before authorization.
- [x] The module fetcher inherits the same behavior (a module URL that redirects off the
      granted host is denied, zero destination requests).
- [x] The prefetch walk stops at its deadline before EVERY fetch: with a WIDE graph
      (one module importing many siblings) and a deliberately slow fake fetcher, the
      number of fetch attempts is bounded well below the graph size and stops growing
      after the deadline, and the outcome is the timeout error.

## Non-goals

- No change to `NetworkPermission` semantics (still host-granular).
- No pinned-IP resolver / DNS-rebinding defense (https scheme + certificate validation
  is the boundary here; embeddings needing more can layer S082-style guards).
- No aggregate byte/heap caps on module graphs (deferred downstream).
- The wasm MCP fetch surface (`simulacra-mcp`'s reqwest client with an
  initial-host-only allowlist and default redirect following) shares this bug
  class but is out of scope here — it needs its own spec before any embedding
  broadens `net:` grants into it.
- Redirect responses surfaced to the caller (a 3xx that IS the final hop result after
  limit-zero following — i.e., no `Location` header) are returned as-is, same as today.

## Test notes

Fake transport implementing `simulacra_http::HttpClient`, recording requests, scripted
per-URL responses. Assert on recorded request counts, not just outcomes — the
"zero destination requests" property is the security claim. The walk-deadline test may
use wall-clock sleeps on the fake fetcher but must assert structurally (attempt count
far below graph size), never on exact timings.
