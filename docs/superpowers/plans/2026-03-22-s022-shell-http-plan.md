# S022 Shell HTTP Builtins (curl/wget) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `curl` and `wget` as shell builtins routing through the `simulacra-http` control plane with capability gating, budget enforcement, journaling, and observability.

**Architecture:** `ShellHttpProxy` trait in simulacra-shell (following FsProxy/FetchProxy pattern), `AgentCellShellHttpProxy` in simulacra-sandbox delegating to `fetch_http_inner()`. curl/wget are builtins in `builtins.rs` that parse flags and call through the proxy. 2-turn budget (shell + HTTP).

**Tech Stack:** Rust, simulacra-shell, simulacra-sandbox, simulacra-http

**Spec:** `docs/superpowers/specs/2026-03-22-s022-shell-http-design.md`

**Key references:**
- `crates/simulacra-shell/src/builtins.rs` — existing builtin registry (18 builtins, match dispatch)
- `crates/simulacra-shell/src/executor.rs:150-190` — `execute_command()` → `try_builtin()` → unknown
- `crates/simulacra-shell/src/lib.rs` — `ShellExecutor`, `CommandResult` types
- `crates/simulacra-sandbox/src/lib.rs:262` — `AgentCell::execute_shell()` constructs `ShellExecutor`
- `crates/simulacra-sandbox/src/fetch_proxy.rs` — `AgentCellFetchProxy` pattern to follow
- `crates/simulacra-sandbox/src/http.rs` — `fetch_http_inner()` Golden Rule chain

---

## File Structure

### New files

| File | Responsibility |
|------|---------------|
| `crates/simulacra-shell/src/http_proxy.rs` | `ShellHttpProxy` trait, `ShellHttpResponse`, `ShellHttpError` |
| `crates/simulacra-sandbox/src/shell_http_proxy.rs` | `AgentCellShellHttpProxy` implementing `ShellHttpProxy` |

Note: `builtin_curl()` and `builtin_wget()` are added directly to `crates/simulacra-shell/src/builtins.rs` (no directory refactor — follow existing pattern).

### Modified files

| File | Change |
|------|--------|
| `crates/simulacra-shell/src/lib.rs` | Add `mod http_proxy`, export `ShellHttpProxy` types, update `ShellExecutor` re-export |
| `crates/simulacra-shell/src/executor.rs` | Add `http_proxy` field to `ShellExecutor`, pass to `try_builtin()` |
| `crates/simulacra-shell/src/builtins.rs` | Add `http_proxy` param to `try_builtin()`, add curl/wget match arms + implementations |
| `crates/simulacra-shell/src/tests.rs` | Add curl/wget tests with `MockShellHttpProxy` |
| `crates/simulacra-sandbox/src/lib.rs` | Add `mod shell_http_proxy`, construct proxy in `execute_shell()`, pass to `ShellExecutor::new()` |
| `crates/simulacra-sandbox/Cargo.toml` | No new deps needed (already has simulacra-shell, simulacra-http) |
| `specs/S002-shell.md` | Add curl/wget to builtin list |
| `specs/SPECS.md` | Add S022 entry |

---

### Task 1: `ShellHttpProxy` trait and types

**Files:**
- Create: `crates/simulacra-shell/src/http_proxy.rs`
- Modify: `crates/simulacra-shell/src/lib.rs`

- [ ] **Step 1: Create `http_proxy.rs` with trait and types**

```rust
// crates/simulacra-shell/src/http_proxy.rs

/// Response from a shell HTTP proxy call.
#[derive(Debug, Clone)]
pub struct ShellHttpResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub url: String,
}

/// Error from a shell HTTP proxy call.
#[derive(Debug)]
pub enum ShellHttpError {
    CapabilityDenied(String),
    BudgetExhausted(String),
    NetworkError(String),
    Timeout,
}

impl std::fmt::Display for ShellHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapabilityDenied(msg) => write!(f, "capability denied: {msg}"),
            Self::BudgetExhausted(msg) => write!(f, "budget exhausted: {msg}"),
            Self::NetworkError(msg) => write!(f, "network error: {msg}"),
            Self::Timeout => write!(f, "operation timed out"),
        }
    }
}

/// Trait for proxying HTTP requests from shell builtins (curl, wget).
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

- [ ] **Step 2: Export from lib.rs**

Add `mod http_proxy;` and `pub use http_proxy::{ShellHttpProxy, ShellHttpResponse, ShellHttpError};` to `crates/simulacra-shell/src/lib.rs`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p simulacra-shell`

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-shell/
git commit -m "feat(shell): add ShellHttpProxy trait and types [S022]"
```

---

### Task 2: Wire `ShellHttpProxy` into `ShellExecutor` and `try_builtin`

**Files:**
- Modify: `crates/simulacra-shell/src/executor.rs`
- Modify: `crates/simulacra-shell/src/builtins.rs`

- [ ] **Step 1: Add `http_proxy` field to `ShellExecutor`**

```rust
pub struct ShellExecutor<'a> {
    vfs: &'a dyn VirtualFs,
    env: HashMap<String, String>,
    http_proxy: Option<&'a dyn ShellHttpProxy>,
}
```

Update `new()`:
```rust
pub fn new(
    vfs: &'a dyn VirtualFs,
    env: HashMap<String, String>,
    http_proxy: Option<&'a dyn ShellHttpProxy>,
) -> Self {
    Self { vfs, env, http_proxy }
}
```

Pass `self.http_proxy` to `builtins::try_builtin()` in `execute_command()`.

- [ ] **Step 2: Update `try_builtin()` signature**

```rust
pub(crate) fn try_builtin(
    program: &str,
    args: &[String],
    stdin: &str,
    vfs: &dyn VirtualFs,
    http_proxy: Option<&dyn ShellHttpProxy>,
) -> Option<CommandResult> {
```

Add match arms for `"curl"` and `"wget"`:
```rust
"curl" => Some(match http_proxy {
    Some(proxy) => builtin_curl(args, stdin, vfs, proxy),
    None => CommandResult::error(1, "curl: network commands require HTTP proxy (not available in this context)\n"),
}),
"wget" => Some(match http_proxy {
    Some(proxy) => builtin_wget(args, stdin, vfs, proxy),
    None => CommandResult::error(1, "wget: network commands require HTTP proxy (not available in this context)\n"),
}),
```

Add stub implementations:
```rust
fn builtin_curl(_args: &[String], _stdin: &str, _vfs: &dyn VirtualFs, _proxy: &dyn ShellHttpProxy) -> CommandResult {
    CommandResult::error(1, "curl: not yet implemented\n")
}
fn builtin_wget(_args: &[String], _stdin: &str, _vfs: &dyn VirtualFs, _proxy: &dyn ShellHttpProxy) -> CommandResult {
    CommandResult::error(1, "wget: not yet implemented\n")
}
```

- [ ] **Step 3: Update all `ShellExecutor::new()` callsites**

Two callsites:
- `crates/simulacra-sandbox/src/lib.rs:262` — pass `None` for now (Task 5 wires the real proxy)
- `crates/simulacra-shell/src/tests.rs:106` — pass `None`

- [ ] **Step 4: Build and test**

Run: `cargo build --workspace && cargo test -p simulacra-shell -p simulacra-sandbox`

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-shell/ crates/simulacra-sandbox/
git commit -m "feat(shell): wire ShellHttpProxy into ShellExecutor and try_builtin [S022]"
```

---

### Task 3: Implement `builtin_curl`

**Files:**
- Modify: `crates/simulacra-shell/src/builtins.rs`
- Modify: `crates/simulacra-shell/src/tests.rs`

- [ ] **Step 1: Write curl tests with `MockShellHttpProxy`**

Create a `MockShellHttpProxy` in tests.rs:
```rust
struct MockShellHttpProxy {
    response: Mutex<Option<Result<ShellHttpResponse, ShellHttpError>>>,
    last_request: Mutex<Option<(String, String, Vec<(String, String)>, Option<Vec<u8>>, Option<u64>)>>,
}
```

Tests:
- `curl_get_returns_body_in_stdout` — `curl http://example.com` → body in stdout, exit 0
- `curl_post_with_data` — `curl -X POST -d "body" URL` → proxy receives POST + body
- `curl_custom_header` — `curl -H "X-Custom: val" URL` → proxy receives header
- `curl_multiple_headers` — `curl -H "A: 1" -H "B: 2" URL`
- `curl_json_shorthand` — `curl --json '{"a":1}' URL` → POST, Content-Type + Accept set, body sent
- `curl_output_to_file` — `curl -o /workspace/out.txt URL` → body written to VFS, no stdout
- `curl_silent` — `curl -s URL` → no progress output in stderr
- `curl_include_headers` — `curl -i URL` → headers prepended to stdout
- `curl_fail_on_error` — `curl -f URL` (proxy returns 404) → exit 1, error in stderr
- `curl_verbose` — `curl -v URL` → request/response headers in stderr
- `curl_connect_timeout` — `curl --connect-timeout 2 URL` → timeout_ms=2000 passed to proxy
- `curl_data_implies_post` — `curl -d "data" URL` without `-X` → method is POST
- `curl_unsupported_flag` — `curl --unsupported URL` → exit 1, supported flags listed
- `curl_capability_denied` — proxy returns CapabilityDenied → exit 1, message in stderr
- `curl_budget_exhausted` — proxy returns BudgetExhausted → exit 1
- `curl_network_error` — proxy returns NetworkError → exit 1
- `curl_timeout_error` — proxy returns Timeout → exit 1
- `curl_no_proxy` — `curl URL` without proxy → exit 1, "network commands require HTTP proxy"
- `curl_http_error_without_fail` — 404 without `-f` → exit 0, body in stdout
- `curl_pipe_to_grep` — `curl URL | grep pattern` → piping works
- `curl_data_raw_sends_body` — `curl --data-raw "body" URL` → same as `-d`, body sent
- `curl_request_long_form` — `curl --request POST URL` → method is POST (alias for `-X`)
- `curl_location_flag_accepted` — `curl -L URL` → accepted without error (no-op)
- `curl_output_vfs_write_error` — `curl -o /nonexistent/path URL` → exit 1, VFS error in stderr
- `curl_redirect_to_file` — `curl URL > /workspace/out.txt` via shell redirect → file written in VFS

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p simulacra-shell -- curl`
Expected: FAIL (stub returns "not yet implemented")

- [ ] **Step 3: Implement `builtin_curl`**

Parse flags from args:
- Iterate args, handle `-X`/`--request`, `-H`/`--header`, `-d`/`--data`, `--data-raw`, `--json`, `-o`/`--output`, `-s`/`--silent`, `-i`/`--include`, `-f`/`--fail`, `-v`/`--verbose`, `-L`/`--location`, `--connect-timeout`
- Last non-flag argument is the URL
- Reject unknown flags with supported list
- Call `proxy.execute(url, method, headers, body, timeout_ms)`
- Format output based on flags:
  - Default: body to stdout
  - `-o file`: write to VFS, transfer summary to stderr (unless `-s`)
  - `-i`: prepend headers
  - `-v`: request/response headers to stderr
  - `-f` + 4xx/5xx: exit 1, error to stderr

- [ ] **Step 4: Run tests**

Run: `cargo test -p simulacra-shell -- curl`
Expected: all pass

- [ ] **Step 5: Clippy + fmt**

Run: `cargo clippy -p simulacra-shell --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-shell/
git commit -m "feat(shell): implement curl builtin with 13 flags [S022]"
```

---

### Task 4: Implement `builtin_wget`

**Files:**
- Modify: `crates/simulacra-shell/src/builtins.rs`
- Modify: `crates/simulacra-shell/src/tests.rs`

- [ ] **Step 1: Write wget tests**

Tests (using same `MockShellHttpProxy`):
- `wget_saves_to_vfs_file` — `wget URL/data.csv` → file `data.csv` in VFS, progress in stderr
- `wget_default_filename_index_html` — `wget URL/` → saves as `index.html`
- `wget_output_document` — `wget -O /workspace/out.txt URL` → saves to specified path
- `wget_stdout_mode` — `wget -O - URL` → body to stdout, no file write
- `wget_quiet` — `wget -q URL` → no stderr progress
- `wget_custom_header` — `wget --header="X-Custom: val" URL`
- `wget_post_data` — `wget --post-data="body" URL` → POST method
- `wget_method_override` — `wget --method=PUT URL`
- `wget_timeout` — `wget --timeout=3 URL` → timeout_ms=3000
- `wget_unsupported_flag` — `wget --unsupported URL` → exit 1, supported flags listed
- `wget_capability_denied` — proxy returns CapabilityDenied → exit 1
- `wget_no_proxy` — without proxy → exit 1, "network commands require HTTP proxy"
- `wget_overwrite_existing` — file exists in VFS, wget overwrites
- `wget_pipe_stdout` — `wget -O - URL | wc -l` → piping works
- `wget_vfs_write_error` — `wget -O /nonexistent/dir/file URL` → exit 1, VFS error in stderr
- `wget_default_progress_output` — `wget URL` without `-q` → stderr contains "Resolving", "HTTP request sent", "Saving to"

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p simulacra-shell -- wget`

- [ ] **Step 3: Implement `builtin_wget`**

Parse flags:
- `-O file` / `--output-document=file` (also `-O -` for stdout)
- `-q` / `--quiet`
- `--header="Name: Value"`
- `--post-data="data"`
- `--method=METHOD`
- `--timeout=SECS`
- `--no-check-certificate` (no-op)
- Last non-flag arg is URL
- Extract filename from URL path (last segment, default `index.html`)
- **Note:** `--header="Name: Value"` requires parsing the `=` to split flag from value, then `:` to split header name from header value within the value. Same for `--post-data=`, `--method=`, `--output-document=`, `--timeout=`.
- Call `proxy.execute(url, method, headers, body, timeout_ms)`
- Write body to VFS file (or stdout if `-O -`)
- Print wget-style progress to stderr (unless `-q`)

- [ ] **Step 4: Run tests**

Run: `cargo test -p simulacra-shell -- wget`

- [ ] **Step 5: Clippy + fmt**

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-shell/
git commit -m "feat(shell): implement wget builtin with 8 flags [S022]"
```

---

### Task 5: Wire `AgentCellShellHttpProxy` in simulacra-sandbox

**Files:**
- Create: `crates/simulacra-sandbox/src/shell_http_proxy.rs`
- Modify: `crates/simulacra-sandbox/src/lib.rs`

- [ ] **Step 1: Implement `AgentCellShellHttpProxy`**

```rust
// crates/simulacra-sandbox/src/shell_http_proxy.rs
use simulacra_shell::{ShellHttpProxy, ShellHttpResponse, ShellHttpError};
use crate::SandboxError;
use crate::http::fetch_http_inner;
// ... capability, budget, journal, agent_id, http_client fields

pub struct AgentCellShellHttpProxy {
    pub capability: CapabilityToken,
    pub budget: Arc<Mutex<ResourceBudget>>,
    pub journal: Arc<dyn JournalStorage>,
    pub agent_id: AgentId,
    pub http_client: Arc<dyn simulacra_http::HttpClient>,
}

impl ShellHttpProxy for AgentCellShellHttpProxy {
    fn execute(&self, url, method, headers, body, timeout_ms) -> Result<ShellHttpResponse, ShellHttpError> {
        // Convert headers from (String, String) to (&str, &str) for fetch_http_inner
        // Call fetch_http_inner with increment_turns: true
        // Map SandboxError to ShellHttpError
        // Map HttpResponse to ShellHttpResponse
    }
}
```

Key: pass `increment_turns: true` (unlike `AgentCellFetchProxy` which passes `false`).

- [ ] **Step 2: Wire into `AgentCell::execute_shell()`**

In `crates/simulacra-sandbox/src/lib.rs`, in `execute_shell()`:
```rust
let shell_http_proxy = AgentCellShellHttpProxy {
    capability: self.capability.clone(),
    budget: Arc::clone(&self.budget),
    journal: Arc::clone(&self.journal),
    agent_id: self.agent_id.clone(),
    http_client: Arc::clone(&self.http_client),
};
let mut executor = simulacra_shell::ShellExecutor::new(
    self.vfs.as_ref(),
    env,
    Some(&shell_http_proxy),
);
```

- [ ] **Step 3: Write integration test**

Test in simulacra-sandbox tests: create `AgentCell` with network capability denied, call `execute_shell("curl http://denied.com")`, verify exit code 1 and "capability denied" in stderr.

Also test: `AgentCell` with network allowed, `execute_shell("curl http://httpbin.org/get")` succeeds (uses real `UreqHttpClient` against localhost test server).

- [ ] **Step 4: Build and test workspace**

Run: `cargo build --workspace && cargo test -p simulacra-sandbox -p simulacra-shell`

- [ ] **Step 5: Clippy + fmt**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-sandbox/
git commit -m "feat(sandbox): add AgentCellShellHttpProxy for curl/wget Golden Rule enforcement [S022]"
```

---

### Task 6: Budget verification and spec updates

**Files:**
- Modify: `crates/simulacra-shell/src/tests.rs` or `crates/simulacra-sandbox/tests/`
- Modify: `specs/S002-shell.md`
- Modify: `specs/SPECS.md`

- [ ] **Step 1: Write budget consumption test**

In simulacra-sandbox tests: create `AgentCell` with `max_turns: 10, used_turns: 0`. Call `execute_shell("curl http://allowed.com")`. Verify `used_turns` is now 2 (1 shell + 1 HTTP).

- [ ] **Step 2: Write observability/journal verification test**

In simulacra-sandbox tests: create `AgentCell` with allowed network capability, call `execute_shell("curl http://localhost:PORT/test")` against a test TCP server. Verify:
- Journal contains a `ShellCommand` entry with `command: "curl ..."`
- Journal contains an `HttpRequest` entry with `url`, `method`, `status`
- Both entries have the correct `agent_id`

This covers spec observability assertions (sandbox_http_fetch span, simulacra_http_request span, journal entries).

- [ ] **Step 3: Update S002-shell.md**

Add `curl` and `wget` to the builtin list. Document `ShellHttpProxy` trait. List supported flags for each command.

- [ ] **Step 4: Add S022 to SPECS.md**

Add row: `| specs/S022-shell-http.md | Active | Shell HTTP builtins (curl/wget) via simulacra-http control plane |`

- [ ] **Step 5: Run full workspace tests**

Run: `cargo test --workspace`

- [ ] **Step 6: Commit**

```bash
git add crates/ specs/
git commit -m "feat(shell): add budget test, update S002 and SPECS.md for curl/wget [S022]"
```
