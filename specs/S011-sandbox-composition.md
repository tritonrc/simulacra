# S011 — Sandbox Composition (Proxy Layer)

**Status:** Active
**Crate:** `simulacra-sandbox`
**Priority:** Critical path for Phase 1

## Dependencies

- **S001** — VFS (raw filesystem operations)
- **S002** — Shell emulator (raw shell execution)
- **S003** — QuickJS runtime (raw JS execution, host functions)
- **S004** — Capability tokens (checked by this proxy)
- **S005** — Journal (entries written by this proxy)
- **S006** — Resource budgets (checked and decremented by this proxy)
- **S010** — Observability conventions (span/metric naming)
- **S014** — ESM modules (ModuleFetcher trait implemented by this proxy)

## Context

`AgentCell` composes VFS + Shell + QuickJS into a single sandboxed environment. Today, `AgentCell` checks capabilities before shell/JS execution but does NOT write journal entries, does NOT check budgets, does NOT emit OTel spans, and does NOT gate VFS operations. The Golden Rule (ARCHITECTURE.md) requires that every side-effecting operation follows the full chain: check capability, check budget, write journal, emit span, execute, return.

This spec defines the **proxy layer** that wraps raw subsystem access so that no operation bypasses the Golden Rule. The proxy is the single enforcement point. Individual subsystems (simulacra-vfs, simulacra-shell, simulacra-quickjs) remain Golden-Rule-unaware — they do their job, the proxy does the governance.

## Design

```
  Tool / Agent Loop
       |
       v
  +---------------------------------------------------+
  |              AgentCell (Proxy Layer)                |
  |                                                     |
  |  For every operation:                               |
  |  1. Check CapabilityToken                           |
  |  2. Check ResourceBudget                            |
  |  3. Create OTel span                                |
  |  4. Execute against raw subsystem                   |
  |  5. Write JournalEntry (after execution, with       |
  |     result metadata such as exit code or status)    |
  |  6. Return result                                   |
  |                                                     |
  |  +----------+ +-----------+ +-----------------+    |
  |  |   VFS    | |   Shell   | |    QuickJS      |    |
  |  |  (raw)   | |  (raw)    | |    (raw)        |    |
  |  +----------+ +-----------+ +-----------------+    |
  +---------------------------------------------------+
```

## Behavior

### Proxy Invariant

1. Every side-effecting operation on `AgentCell` follows the Golden Rule sequence: capability check -> budget check -> span creation -> execution -> journal write (with result) -> result return. No exceptions.
2. If capability check fails, the operation returns `SandboxError::CapabilityDenied` immediately. No budget is consumed. A journal entry is still written (recording the denial). A WARN-level OTel event is emitted.
3. If budget check fails, the operation returns `SandboxError::BudgetExhausted` immediately. A journal entry is still written (recording the exhaustion). A WARN-level OTel event is emitted.
4. The raw VFS, Shell, and QuickJS subsystems are never exposed directly. All access goes through `AgentCell` methods. The subsystems themselves do not check capabilities, budgets, or write journals.

### Budget Increment Timing

5. Budget counters (`used_turns`, `used_vfs_bytes`) are incremented **before** execution so the cost is paid even if the operation crashes or is interrupted.

### Journal Timing

6. Journal entries for reads, shell commands, JS execution, and HTTP requests are written **after** execution completes, because the journal entry includes result metadata (e.g., exit code, HTTP status, bytes read). File write journal entries are written before execution (they record intent, not result). Capability denials and budget exhaustions are journaled immediately (before returning the error).

### AgentCell Construction

6. `AgentCell` is constructed with: `Arc<dyn VirtualFs>`, `CapabilityToken`, `ResourceBudget`, and `Arc<dyn JournalStorage>`.
7. Each `AgentCell` holds exactly one `ShellExecutor` and exactly one QuickJS runtime. These are not shared across cells. They are initialized during `AgentCell::new` and persist for the lifetime of the cell.

### File Operations (VFS proxy)

8. `read_file(path)` checks `paths_read` capability, emits a span, delegates to `VFS::read()`, then writes a `JournalEntryKind::ToolResult` (recording the read and bytes returned). It does not consume or gate on `tokens`/`turns`; those budgets are enforced at the LLM turn and active tool operation boundaries.
9. `write_file(path, data)` checks `paths_write` capability, checks and reserves VFS byte budget only, writes a `JournalEntryKind::FileWrite` (before execution, recording intent), emits a span, then delegates to `VFS::write()`. It does not gate on exhausted LLM token budget.
10. `list_dir(path)` checks `paths_read` capability, emits a span, then delegates to `VFS::list_dir()`. No journal entry (read-only, non-mutating metadata query). `list_dir` does NOT consume a `turns` budget unit -- it is a metadata query, not a tool invocation.
11. Path capability checking uses glob matching: a token with `paths_read: ["/**"]` grants read to all paths; `paths_write: ["/workspace/**", "/output/**"]` restricts writes to those subtrees.

### Shell Operations (Shell proxy)

12. `execute_shell(command)` checks `shell` capability, checks budget (`turns` counter), emits a span, delegates to `ShellExecutor::run()`, then writes a `JournalEntryKind::ShellCommand` with the command and exit code (journaled after execution because the exit code is only known after the command completes). Increments `used_turns` by 1.
13. Shell commands that produce VFS writes (e.g. `echo foo > /bar`) go through the shell emulator's VFS reference -- which IS the same `Arc<dyn VirtualFs>` the proxy holds. The shell emulator's internal writes are NOT individually journaled (the shell command as a whole is the journaled unit).

### JavaScript Operations (QuickJS proxy)

14. `execute_js(code)` checks `javascript` capability, checks budget (`turns` counter), emits a span, delegates to the QuickJS runtime, then writes a `JournalEntryKind::CodeExecution { language: "javascript" }` (journaled after execution). Increments `used_turns` by 1.
15. If QuickJS is not yet integrated (stub phase), `execute_js` returns `SandboxError::NotImplemented` after capability/budget checks pass.

### JS Host Function Callback Mechanism

16. QuickJS host functions (`fs.readFileSync`, `fs.writeFileSync`, and `simulacra:fs` module functions) must NOT call VFS directly. Instead, they route through `AgentCell` proxy methods (`read_file`, `write_file`) so that capability checks, budget checks, journal writes, and span emission all apply to file operations initiated from JS code.
17. The callback mechanism works as follows: `AgentCell` creates the `JsRuntime` and registers closures that capture references back to the `AgentCell` proxy methods. When JS code calls `fs.readFileSync(path)`, the host function closure calls `AgentCell::read_file(path)`, which enforces the full Golden Rule chain. The JS host function converts the `SandboxError` result into a JS exception if the operation is denied.
18. `console.log` is NOT proxied through the Golden Rule chain. It writes to the agent's virtual stdout buffer directly. It is not a side-effecting operation (no external state change, no capability needed, no budget consumed, no journal entry).

### HTTP Operations (HTTP proxy)

19. `fetch_http(url, method, headers, body)` checks `network` capability (against the URL's host), checks budget (`turns` counter), emits a span, delegates to the HTTP client (`reqwest`), then writes a `JournalEntryKind::HttpRequest { method, url, status }` (journaled after execution because the HTTP status is only known after the response). Increments `used_turns` by 1.
20. `fetch_http` returns a structured response containing: HTTP status code, response headers (as key-value pairs), and response body (as bytes or string).
21. If the `network` capability denies the URL's host, the operation returns `SandboxError::CapabilityDenied` immediately with reason indicating the denied host.
22. Network errors (DNS failure, timeout, connection refused) return `SandboxError::Http(String)` with a message including the URL and the failure reason.

### ModuleFetcher Integration

23. `AgentCell` provides a `ModuleFetcher` implementation (see S014) to the `JsRuntime` it owns. When JS code imports an `https://` module, the QuickJS module loader calls `ModuleFetcher::fetch(url)`, which delegates to `AgentCell::fetch_http`. This ensures remote module fetches go through the full Golden Rule chain: capability check (network permission against the URL), budget check, journal write (`HttpRequest`), span emission.
24. The `ModuleFetcher` impl is constructed during `AgentCell` initialization and passed to `JsRuntime::with_fetcher()` (or `JsRuntime::with_timeout_and_fetcher()`). The fetcher holds a reference (or `Arc`) back to the `AgentCell`'s proxy methods.

### Budget Enforcement

25. `AgentCell` holds a mutable reference (or `Arc<Mutex<ResourceBudget>>`) to the budget. Each operation that consumes a budget resource decrements the appropriate counter.
26. `execute_shell`, `execute_js`, and `fetch_http` each consume one `turns` unit from the budget. The field is `ResourceBudget::used_turns` / `ResourceBudget::max_turns`.
27. `write_file` adds the written byte count to `used_vfs_bytes`.
28. Budget exhaustion produces a structured error: `SandboxError::BudgetExhausted { resource, used, limit }`.

### Error Types

29. `SandboxError` enum includes: `CapabilityDenied`, `BudgetExhausted`, `Shell(String)`, `Js(String)`, `Vfs(FsError)`, `Http(String)`, `NotImplemented(String)`, `Journal(JournalError)`.
30. Journal write failures do NOT prevent the operation from executing. The operation proceeds, and the journal failure is logged at ERROR level. Rationale: a journal write failure is an infrastructure problem, not a policy violation -- the agent should not be punished for it.

### Thread Safety

31. `AgentCell` is `Send` but NOT required to be `Sync`. Each agent task owns its cell exclusively. No shared mutable state across tasks.

## Assertions

### Golden Rule enforcement

- [x] `read_file` with denied `paths_read` returns `CapabilityDenied` and does NOT read from VFS.
- [x] `write_file` with denied `paths_write` returns `CapabilityDenied` and does NOT write to VFS.
- [x] `execute_shell` with `shell: false` returns `CapabilityDenied` and does NOT execute the command.
- [x] `execute_js` with `javascript: false` returns `CapabilityDenied` and does NOT execute JS.
- [x] `write_file` when `vfs_bytes` budget is exhausted returns `BudgetExhausted` and does NOT write.
- [x] `read_file` and `write_file` are not rejected merely because the LLM token budget is exhausted; `write_file` still enforces `vfs_bytes`. **Tested in `sandbox_budget_scopes.rs`.**
- [x] `execute_shell` when `turns` budget is exhausted returns `BudgetExhausted` and does NOT execute.
- [x] `execute_js` when `turns` budget is exhausted returns `BudgetExhausted` and does NOT execute.
- [x] `fetch_http` with denied `network` capability returns `CapabilityDenied` and does NOT make an HTTP request. **Tested in `fetch_http_with_denied_network_capability_returns_capability_denied_and_does_not_make_a_request`.**
- [x] `fetch_http` when `turns` budget is exhausted returns `BudgetExhausted` and does NOT make an HTTP request. **Tested in `fetch_http_when_turns_budget_is_exhausted_returns_budget_exhausted_and_does_not_make_a_request`.**

### Journal integration

- [x] `write_file` writes a `FileWrite` journal entry with path and size before returning.
- [x] `execute_shell` writes a `ShellCommand` journal entry with command and exit code after execution.
- [x] `execute_js` writes a `CodeExecution` journal entry with `language: "javascript"` after execution.
- [x] Capability denial writes a journal entry recording the denied operation and reason.
- [x] Budget exhaustion writes a journal entry recording the exhausted resource.
- [x] `fetch_http` writes an `HttpRequest` journal entry with method, url, and status after execution. **Tested in `fetch_http_writes_an_httprequest_journal_entry_with_method_url_and_status_after_execution`.**

### Budget accounting

- [x] `execute_shell` increments `used_turns` by 1.
- [x] `execute_js` increments `used_turns` by 1.
- [x] `write_file` increments `used_vfs_bytes` by the written byte count.
- [x] Budget with limit 0 means unlimited -- operations succeed regardless of usage count.
- [x] `fetch_http` increments `used_turns` by 1. **Tested in `fetch_http_increments_used_turns_by_one`.**
- [x] `list_dir` does NOT increment `used_turns`. **Tested in `list_dir_does_not_increment_used_turns`.**
- [x] `execute_shell`, `execute_js`, and `fetch_http` all increment `used_turns` **before** execution, not after. **Tested in `execute_shell_execute_js_and_fetch_http_all_increment_used_turns_before_execution_not_after`.**

### Path capability matching

- [x] `paths_read: ["/workspace/**"]` allows `read_file("/workspace/foo.txt")`.
- [x] `paths_read: ["/workspace/**"]` denies `read_file("/secrets/key.pem")`.
- [x] `paths_write: ["/output/**"]` allows `write_file("/output/result.txt", data)`.
- [x] `paths_write: ["/output/**"]` denies `write_file("/workspace/sneaky.txt", data)`.
- [x] `paths_read: ["/**"]` (wildcard root) allows reading any path.
- [x] Empty `paths_read` denies all reads. Empty `paths_write` denies all writes.

### Construction and ownership

- [x] `AgentCell::new` accepts VFS, CapabilityToken, ResourceBudget, and JournalStorage.
- [x] `AgentCell` is `Send`.
- [x] Two `AgentCell` instances with different VFS references do not share filesystem state.
- [x] `AgentCell` holds a persistent `ShellExecutor` — shell state (CWD, env vars) survives across `execute_shell` calls within the same cell.
- [x] `AgentCell` holds a persistent `JsRuntime` wrapper for host configuration and remote source caches. Each `execute_js` call uses a fresh QuickJS runtime/context, so JS globals and module instances do not survive across calls. **S053 owns the async runtime v2 substrate.**

### JS host function callback routing

- [x] `fs.readFileSync` from JS code routes through `AgentCell::read_file`, not directly to VFS. A denied path returns a JS exception. **Tested in `fs_readfilesync_from_js_code_routes_through_agent_cell_read_file_and_denied_paths_return_a_js_exception`.**
- [x] `fs.writeFileSync` from JS code routes through `AgentCell::write_file`, not directly to VFS. A denied path returns a JS exception. **Tested in `fs_writefilesync_from_js_code_routes_through_agent_cell_write_file_and_denied_paths_return_a_js_exception`.**
- [x] `simulacra:fs` `readFile` and `writeFile` also route through `AgentCell` proxy methods. **Tested in `simulacra_fs_readfile_and_writefile_also_route_through_agent_cell_proxy_methods`.**
- [x] `console.log` does NOT route through the proxy -- it writes directly to the virtual stdout buffer. **Tested in `console_log_does_not_route_through_the_proxy_and_writes_directly_to_the_virtual_stdout_buffer`.**

### ModuleFetcher integration

- [x] `AgentCell` provides a `ModuleFetcher` impl to the `JsRuntime` it owns. **Tested in `agent_cell_provides_a_modulefetcher_impl_to_the_js_runtime_it_owns`.**
- [x] Remote module `import "https://..."` triggers `ModuleFetcher::fetch`, which delegates to `AgentCell::fetch_http`. **Tested in `remote_module_import_triggers_modulefetcher_fetch_which_delegates_to_agent_cell_fetch_http`.**
- [x] Remote module fetch with denied network capability fails with a capability error message surfaced as a JS module loading error. **Tested in `remote_module_fetch_with_denied_network_capability_fails_with_a_capability_error_message_surfaced_as_a_js_module_loading_error`.**

### HTTP operations

- [x] `fetch_http` to an allowed host returns the HTTP response with status, headers, and body. **Tested in `fetch_http_to_an_allowed_host_returns_the_http_response_with_status_headers_and_body`.**
- [x] `fetch_http` to a denied host returns `SandboxError::CapabilityDenied`. **Tested in `fetch_http_to_a_denied_host_returns_sandboxerror_capability_denied`.**
- [x] `fetch_http` network error (e.g., DNS failure) returns `SandboxError::Http` with URL and failure reason. **Tested in `fetch_http_network_error_returns_http_with_the_url_and_the_failure_reason`.**

### Error propagation

- [x] VFS error (e.g. read non-existent file) propagates as `SandboxError::Vfs`.
- [x] Shell error (e.g. command not found) returns the shell result (exit code != 0), not a SandboxError.
- [x] Journal write failure does NOT prevent the operation from executing -- operation succeeds and journal failure is logged at ERROR.

## Observability (see S010 for conventions)

- [x] `read_file` produces a span with `simulacra.operation.name` = `sandbox_read_file` and `simulacra.vfs.path`.
- [x] `write_file` produces a span with `simulacra.operation.name` = `sandbox_write_file`, `simulacra.vfs.path`, and `simulacra.vfs.bytes`.
- [x] `execute_shell` produces a span with `simulacra.operation.name` = `sandbox_shell_exec` and `simulacra.shell.command`.
- [x] `execute_js` produces a span with `simulacra.operation.name` = `sandbox_js_exec`.
- [x] Capability denials emit a `WARN`-level event on the current span with `simulacra.capability.operation` and `simulacra.capability.reason`.
- [x] Budget exhaustion emits a `WARN`-level event on the current span with `simulacra.budget.resource`, `simulacra.budget.used`, and `simulacra.budget.limit`.
- [x] `fetch_http` produces a span with `simulacra.operation.name` = `sandbox_http_fetch`, `simulacra.http.url`, `simulacra.http.method`, and `simulacra.http.status`. **Tested in `fetch_http_produces_a_sandbox_http_fetch_span_with_url_method_and_status`.**
