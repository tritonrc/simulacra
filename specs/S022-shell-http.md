# S022 — Shell HTTP Builtins (curl/wget)

**Status:** Active
**Crates involved:** `simulacra-shell`, `simulacra-sandbox`

## Dependencies

- **S002** — Shell emulator: builtins, pipes, redirects
- **S004** — Capability tokens: network permission gating
- **S006** — Resource budgets: turns consumption
- **S011** — Sandbox: Golden Rule enforcement
- **S021** — HTTP control plane (simulacra-http, simulacra-sandbox HTTP infrastructure)

## Scope

Add `curl` and `wget` as shell builtins routing through the `simulacra-http` control plane. Same governed surface as JS `fetch()` — capability gating, budget enforcement, journaling, observability.

Full design: `docs/superpowers/specs/2026-03-22-s022-shell-http-design.md`

## Behavior

### ShellHttpProxy trait

- [x] `ShellHttpProxy` trait is defined in `simulacra-shell` with an `execute(url, method, headers, body, timeout_ms)` method.
- [x] `ShellExecutor::new` accepts an optional `&dyn ShellHttpProxy`.
- [x] `curl` without a proxy returns exit code 1 with a `"network commands require HTTP proxy"` stderr message.
- [x] `wget` without a proxy returns exit code 1 with the same no-proxy stderr message.

### AgentCellShellHttpProxy (sandbox wiring)

- [x] `AgentCellShellHttpProxy` in `simulacra-sandbox` implements `ShellHttpProxy` by delegating to `fetch_http_inner` with `increment_turns: true`.
- [x] Capability denial from the sandbox maps to `ShellHttpError::CapabilityDenied`.
- [x] Budget exhaustion from the sandbox maps to `ShellHttpError::BudgetExhausted`.
- [x] Underlying network failure maps to `ShellHttpError::NetworkError`.
- [x] Upstream timeout maps to `ShellHttpError::Timeout`.
- [ ] The proxy is constructed and passed in `AgentCell::execute_shell` with the same lifetime as the VFS borrow (no `Arc` required).

### curl builtin — HTTP method and body

- [x] `curl URL` issues GET and prints the response body to stdout with exit code 0.
- [x] `curl -X POST -d "data" URL` sends POST with body.
- [x] `curl -d "data" URL` implies POST method when no `-X` is specified.
- [x] `curl --data-raw "data" URL` sends the string as a literal body (no file expansion).
- [x] `curl --request METHOD URL` (long form) is equivalent to `-X METHOD`.
- [x] `curl --json '{"a":1}' URL` sets body, `Content-Type: application/json`, `Accept: application/json`, and implies POST.

### curl builtin — headers, output, verbosity

- [x] `curl -H "Name: Value" URL` adds a custom request header.
- [x] `curl -H "A: 1" -H "B: 2" URL` accepts multiple `-H` flags.
- [x] `curl -o file URL` writes the response body to the VFS path and keeps stdout clean.
- [x] `curl -s URL` suppresses non-body output (progress, warnings).
- [x] `curl -i URL` prepends the response status line and headers before the body.
- [x] `curl -v URL` prints request and response headers to stderr.
- [x] `curl -L URL` accepts `--location` (no-op — simulacra-http follows redirects by default).
- [x] `curl --connect-timeout 2 URL` passes `timeout_ms = 2000` to the proxy.

### curl builtin — error behaviour

- [x] `curl URL` returning HTTP 4xx/5xx without `-f` still exits 0 (the response body is valid output).
- [x] `curl -f URL` on HTTP 4xx/5xx exits 1 with an HTTP error in stderr.
- [x] `curl --unsupported-flag URL` exits 1 with a stderr message listing supported flags.
- [x] Capability-denied curl request exits 1 with `"curl: capability denied"` in stderr.
- [x] Budget-exhausted curl request exits 1 with `"curl: budget exhausted"` in stderr.
- [x] Network-error curl request exits 1 with `"curl: network error"` in stderr.
- [x] Timeout curl request exits 1 with `"curl: operation timed out"` in stderr.
- [x] `curl -o file URL` with a denied VFS path exits 1 with a VFS error in stderr.

### wget builtin — URL → file behaviour

- [x] `wget URL` with a path segment saves the body to a VFS file named from that segment (e.g. `/data.csv` → `data.csv`).
- [x] `wget URL` with path `/` saves the body to `index.html`.
- [x] `wget -O output.txt URL` writes the body to the given VFS path.
- [x] `wget -O - URL` writes the body to stdout instead of a file.
- [x] `wget URL` over an existing file overwrites silently.

### wget builtin — method and headers

- [x] `wget --post-data="data" URL` sends a POST with the given body.
- [x] `wget --method=DELETE URL` overrides the HTTP method.
- [x] `wget --header="X-Custom: v" URL` adds a custom request header.
- [x] `wget --timeout=2 URL` passes `timeout_ms = 2000` to the proxy.
- [x] `wget --no-check-certificate URL` is accepted (no-op — TLS is handled by the HTTP client).

### wget builtin — output and errors

- [x] `wget -q URL` suppresses the progress output.
- [x] `wget URL` default (non-quiet) prints a progress-style summary to stderr.
- [x] `wget --unsupported-flag URL` exits 1 with a stderr message listing supported flags.
- [x] Capability-denied wget request exits 1 with `"wget: capability denied"` in stderr.
- [x] `wget -O path URL` with a denied VFS path exits 1 with a VFS error in stderr.

### Shell integration

- [x] Multiline `curl` command with line continuations (`\\` backslash) parses as a single invocation.
- [ ] `curl URL | grep pattern` pipes stdout into the next pipeline stage.
- [ ] `wget -O - URL | wc -l` pipes stdout into the next pipeline stage.
- [ ] `curl URL > file.txt` writes stdout through the shell redirect into the VFS.

### Budget consumption

- [ ] A `curl URL` invocation consumes 2 turns total (1 shell + 1 HTTP) from the parent budget.
- [ ] A `wget URL` invocation consumes 2 turns total (1 shell + 1 HTTP).

## Observability (see S010)

No new spans or metrics — curl/wget ride the existing HTTP + shell observability:

- [ ] Shell invocation produces a `shell_command` span (from `ShellExecutor::execute_pipeline`).
- [ ] HTTP request produces a `sandbox_http_fetch` span nested under the shell span.
- [ ] HTTP request produces a `simulacra_http_request` span nested under the sandbox span.
- [ ] Journal records both a `ShellCommand` entry and an `HttpRequest` entry for each curl/wget invocation.
