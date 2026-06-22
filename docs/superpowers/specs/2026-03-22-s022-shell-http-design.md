# S022 — Shell HTTP Builtins (curl/wget)

**Status:** Active
**Crates involved:** `simulacra-shell`, `simulacra-sandbox`

## Dependencies

- **S002** — Shell emulator: builtins, pipes, redirects
- **S004** — Capability tokens: network permission gating
- **S006** — Resource budgets: turns consumption
- **S011** — Sandbox: Golden Rule enforcement
- **S021** — HTTP control plane (`simulacra-http::HttpClient`, `simulacra-sandbox::fetch_http_inner`)

## Scope

Add `curl` and `wget` as shell builtins that route through the `simulacra-http` control plane. All HTTP requests from the shell go through the same governed surface as JS `fetch()` — capability gating, budget enforcement, journaling, observability.

**In scope:**
- `curl` builtin with ~13 commonly-used flags
- `wget` builtin with ~8 commonly-used flags
- `ShellHttpProxy` trait in simulacra-shell (following FsProxy/FetchProxy pattern)
- `AgentCellShellHttpProxy` implementation in simulacra-sandbox
- VFS file output for `curl -o` and `wget` default behavior

**Out of scope:**
- `httpie` (`http`/`https` commands) — never seen in agent usage
- Full curl/wget flag compatibility — unsupported flags return a helpful error
- Cookie jars, client certificates, proxy settings — enterprise additions later
- FTP, SCP, or other non-HTTP protocols
- Resume/retry (`wget -c`, `curl --retry`)

## Context

Agents generate `curl` and `wget` commands frequently. Today these fail with "command not found" because simulacra-shell has no network builtins. The agent must use JS `fetch()` instead, which is unnatural for shell-oriented tasks.

With `simulacra-http` (S021) established as the HTTP control plane, routing shell HTTP commands through it is straightforward. The `ShellHttpProxy` trait follows the same pattern as `FsProxy` (filesystem) and `FetchProxy` (JS fetch) — a trait in the downstream crate, implementation in simulacra-sandbox.

## Design

### Architecture

```text
Agent shell command: curl -X POST -H "Content-Type: application/json" -d '{"key":"val"}' https://api.example.com/data

ShellExecutor::run()
    │
    ▼
execute_command("curl", args)
    │
    ▼
builtin_curl(args, vfs, http_proxy)
    │  ├─ Parse flags (-X, -H, -d, -o, -s, -i, -f, -v, etc.)
    │  ├─ Construct URL, method, headers, body
    │  └─ Call http_proxy.execute(url, method, headers, body, timeout_ms)
    │
    ▼
ShellHttpProxy::execute()           ← trait defined in simulacra-shell
    │
    ▼
AgentCellShellHttpProxy             ← impl in simulacra-sandbox
    │
    ▼
fetch_http_inner()                  ← Golden Rule chain
    │  ├─ span (sandbox_http_fetch)
    │  ├─ capability check (network permission)
    │  ├─ budget check (turns)
    │  ├─ HttpClient::execute() → ureq
    │  ├─ journal (HttpRequest entry)
    │  └─ return
    │
    ▼
ShellHttpResponse → format output → CommandResult
```

### Budget model

Each `curl`/`wget` invocation consumes **2 turns**:
1. Shell execution turn — incremented by `AgentCell::execute_shell()` before calling `ShellExecutor::run()`
2. HTTP call turn — incremented by `fetch_http_inner()` inside the Golden Rule chain

This is consistent with the JS path where `execute_js("await fetch(...)")` also costs 2 turns (1 for JS, 1 for fetch).

## Behavior

### ShellHttpProxy trait

1. `ShellHttpProxy` trait defined in `simulacra-shell`:
    ```rust
    pub trait ShellHttpProxy: Send + Sync {
        fn execute(
            &self,
            url: &str,
            method: &str,
            headers: &[(String, String)],
            body: Option<&[u8]>,
            timeout_ms: Option<u64>,
        ) -> Result<ShellHttpResponse, ShellHttpError>;
    }
    ```
2. `ShellHttpResponse` contains: `status: u16`, `status_text: String`, `headers: Vec<(String, String)>`, `body: Vec<u8>`, `url: String`.
3. `ShellHttpError` enum: `CapabilityDenied(String)`, `BudgetExhausted(String)`, `NetworkError(String)`, `Timeout`.
4. `ShellExecutor` gains `http_proxy: Option<&dyn ShellHttpProxy>` field, passed to `try_builtin()`.
5. When `curl` or `wget` is invoked without an HTTP proxy, return exit code 1 with stderr: `"<command>: network commands require HTTP proxy (not available in this context)"`.

### AgentCellShellHttpProxy

6. `AgentCellShellHttpProxy` in `simulacra-sandbox` implements `ShellHttpProxy` by calling `fetch_http_inner()` with `increment_turns: true`. This differs from `AgentCellFetchProxy` (which passes `false` because `execute_js` already claimed the turn). For the shell path, the shell turn and HTTP turn are independent — both must increment.
7. Error mapping: `SandboxError::CapabilityDenied` → `ShellHttpError::CapabilityDenied`, `BudgetExhausted` → `ShellHttpError::BudgetExhausted`, `Http` → `ShellHttpError::NetworkError`.
8. Response mapping: `simulacra_http::HttpResponse` fields copied to `ShellHttpResponse`.
9. Constructed as a local in `AgentCell::execute_shell()` from AgentCell's fields (capability, budget, journal, agent_id, http_client). Passed to `ShellExecutor::new()` as `&proxy` — the proxy shares the `'a` lifetime with the VFS borrow, both scoped to the `execute_shell()` call. No `Arc` needed.

### curl builtin

10. `curl URL` — GET request, response body to stdout, exit code 0.
11. `-X METHOD` / `--request METHOD` — set HTTP method. Supported: GET, HEAD, POST, PUT, PATCH, DELETE.
12. `-H "Name: Value"` / `--header "Name: Value"` — add request header. Repeatable for multiple headers.
13. `-d "data"` / `--data "data"` — send request body. Implies POST if no `-X` specified. Note: unlike real curl, `-d @filename` does NOT read from VFS — the `@filename` string is sent as literal data. This is a deliberate simplification; use `curl -d "$(cat file)" URL` to send file contents.
14. `--data-raw "data"` — identical to `-d` (both treat data as literal). Accepted for compatibility.
15. `--json '{"key":"val"}'` — shorthand: sets body, `Content-Type: application/json`, and `Accept: application/json`. Implies POST if no `-X`.
16. `-o file` / `--output file` — write response body to VFS file instead of stdout. Print transfer summary to stderr (unless `-s`).
17. `-s` / `--silent` — suppress all non-body output (progress, errors).
18. `-i` / `--include` — prepend HTTP response headers before body in stdout: `"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n...\r\n\r\n<body>"`.
19. `-f` / `--fail` — on HTTP 4xx/5xx, return exit code 1 with error in stderr instead of outputting the error body.
20. `-v` / `--verbose` — print request and response headers to stderr: `"> GET / HTTP/1.1\n> Host: example.com\n..."` and `"< HTTP/1.1 200 OK\n< Content-Type: text/html\n..."`.
21. `-L` / `--location` — follow redirects. Accepted but no-op (simulacra-http follows redirects by default).
22. `--connect-timeout SECS` — map to `timeout_ms` on the proxy (seconds × 1000).
23. Unsupported flags return exit code 1 with stderr: `"curl: unsupported option '<flag>'. Supported: -X, -H, -d, --data-raw, --json, -o, -s, -i, -f, -v, -L, --connect-timeout"`.

### wget builtin

24. `wget URL` — GET request, save response body to VFS file named from URL's last path segment. Default filename: `index.html` if path ends with `/` or is empty.
25. `-O file` / `--output-document=file` — write to specific VFS path. `-O -` writes to stdout instead of file.
26. `-q` / `--quiet` — suppress all output except errors.
27. `--header="Name: Value"` — add request header. Repeatable.
28. `--post-data="data"` — POST with body.
29. `--method=METHOD` — set HTTP method.
30. `--timeout=SECS` — map to `timeout_ms`.
31. `--no-check-certificate` — accepted but no-op (TLS handled by ureq).
32. Unsupported flags return exit code 1 with stderr: `"wget: unsupported option '<flag>'. Supported: -O, -q, --header, --post-data, --method, --timeout"`.
33. Default (non-quiet) output to stderr:
    ```
    Resolving example.com... connecting.
    HTTP request sent, awaiting response... 200 OK
    Length: 1234 [text/html]
    Saving to: 'index.html'

    'index.html' saved [1234]
    ```
34. If output file already exists in VFS, overwrite silently.

### Output and piping

35. `curl URL | grep pattern` — works naturally (stdout piped to next command).
36. `wget -O - URL | wc -l` — stdout mode, piped to wc.
37. `curl URL > file.txt` — shell redirect writes stdout to VFS (existing redirect handling, not curl's concern).
38. VFS write errors (path denied, disk full) return exit code 1 with error in stderr.

### Error handling

39. `ShellHttpError::CapabilityDenied` → exit code 1, stderr: `"<command>: capability denied: <message>"`.
40. `ShellHttpError::BudgetExhausted` → exit code 1, stderr: `"<command>: budget exhausted: <message>"`.
41. `ShellHttpError::NetworkError` → exit code 1, stderr: `"<command>: network error: <message>"`.
42. `ShellHttpError::Timeout` → exit code 1, stderr: `"<command>: operation timed out"`.
43. HTTP 4xx/5xx without `-f` — exit code 0 (matches real curl behavior; the response is valid, just an error status).
44. HTTP 4xx/5xx with `-f` (curl) — exit code 1, stderr: `"curl: HTTP error: <status> <status_text>"`.

### ShellExecutor changes

45. `ShellExecutor::new()` gains `http_proxy: Option<&'a dyn ShellHttpProxy>` parameter.
46. `try_builtin()` gains `http_proxy: Option<&dyn ShellHttpProxy>` parameter.
47. All existing `ShellExecutor::new()` callsites updated to pass `None` (or the proxy when in AgentCell context).

## Assertions

### ShellHttpProxy trait

- [ ] `ShellHttpProxy` trait is defined in `simulacra-shell` with `execute()` method.
- [ ] `ShellExecutor` accepts optional `ShellHttpProxy`.
- [ ] `curl` without proxy returns exit code 1 with "network commands require HTTP proxy" stderr.
- [ ] `wget` without proxy returns exit code 1 with same message.

### AgentCellShellHttpProxy

- [ ] `AgentCellShellHttpProxy` implements `ShellHttpProxy` by delegating to `fetch_http_inner()`.
- [ ] Capability denial maps to `ShellHttpError::CapabilityDenied`.
- [ ] Budget exhaustion maps to `ShellHttpError::BudgetExhausted`.
- [ ] Network error maps to `ShellHttpError::NetworkError`.
- [ ] Constructed and passed in `AgentCell::execute_shell()`.

### curl builtin

- [ ] `curl URL` returns response body in stdout, exit code 0.
- [ ] `curl -X POST -d "body" URL` sends POST with body.
- [ ] `curl -H "Content-Type: application/json" URL` sends custom header.
- [ ] `curl --json '{"a":1}' URL` sets Content-Type, Accept, and body; implies POST.
- [ ] `curl -o file.txt URL` writes body to VFS, not stdout.
- [ ] `curl -s URL` suppresses non-body output.
- [ ] `curl -i URL` includes response headers before body.
- [ ] `curl -f URL` (with 404 response) returns exit code 1.
- [ ] `curl -v URL` prints request/response headers to stderr.
- [ ] `curl --connect-timeout 2 URL` passes timeout_ms=2000 to proxy.
- [ ] `curl --unsupported-flag URL` returns exit code 1 with supported flags list.
- [ ] `curl -d "data" URL` implies POST method when no `-X`.

### wget builtin

- [ ] `wget URL` saves body to VFS file named from URL path.
- [ ] `wget -O output.txt URL` saves to specified VFS path.
- [ ] `wget -O - URL` writes body to stdout.
- [ ] `wget -q URL` suppresses progress output.
- [ ] `wget --post-data="data" URL` sends POST.
- [ ] `wget --header="X-Custom: val" URL` sends custom header.
- [ ] `wget --timeout=2 URL` passes timeout_ms=2000 to proxy.
- [ ] `wget URL` with path `/data.csv` saves as `data.csv`.
- [ ] `wget URL` with path `/` saves as `index.html`.
- [ ] `wget --unsupported-flag URL` returns exit code 1 with supported flags list.

### Error handling

- [ ] Capability denied returns exit code 1 with `"capability denied"` in stderr.
- [ ] Budget exhausted returns exit code 1 with `"budget exhausted"` in stderr.
- [ ] Network error returns exit code 1 with `"network error"` in stderr.
- [ ] Timeout returns exit code 1 with `"operation timed out"` in stderr.

### Piping and integration

- [ ] `curl URL | grep pattern` pipes stdout correctly.
- [ ] `wget -O - URL | wc -l` pipes stdout correctly.
- [ ] VFS write failure returns exit code 1.

### Budget

- [ ] `curl URL` consumes 2 turns total (1 shell + 1 HTTP).

### Observability

- [ ] HTTP calls from curl/wget produce `sandbox_http_fetch` span (via `fetch_http_inner`).
- [ ] HTTP calls produce `simulacra_http_request` span (via `HttpClient::execute`).
- [ ] Shell command produces `shell_command` span wrapping the HTTP spans.
- [ ] Journal records both `ShellCommand` entry and `HttpRequest` entry.

## Observability (see S010)

No new spans or metrics. The existing observability stack covers shell HTTP builtins:
- `shell_command` span from `ShellExecutor::execute_pipeline()`
- `sandbox_http_fetch` span from `fetch_http_inner()`
- `simulacra_http_request` span from `UreqHttpClient::execute()`
- `JournalEntryKind::ShellCommand` from `AgentCell::execute_shell()`
- `JournalEntryKind::HttpRequest` from `fetch_http_inner()`
