# S003 — QuickJS Runtime

**Status:** Active
**Crate:** `simulacra-quickjs`

## Behavior

1. Each `AgentCell` gets exactly one `JsRuntime` wrapper for host configuration and caches, but each `eval` creates a fresh QuickJS runtime/context. JS globals, prototypes, monkey patches, and module instances are never shared across tool calls.
2. JS code runs in ESM mode. `require()` is not available.
3. Host functions (`fs.readFileSync`, `fs.writeFileSync`, `console.log`) are implemented in Rust, not JS polyfills.
4. **Golden Rule delegation:** `simulacra-quickjs` itself does NOT check capabilities, budgets, or write journal entries. It is a pure execution environment. All Golden Rule enforcement happens at the `AgentCell` proxy layer (see S011). Host functions that perform side effects (fs, fetch) call back through `AgentCell` proxy methods, which enforce the full chain.
5. `console.log(...)` writes to the agent's virtual stdout, not real stdout. Output formatting follows Node.js/browser conventions (not `JSON.stringify`, not Rust Debug):
   - Strings: bare (no quotes) at top level, single-quoted when nested inside arrays/objects.
   - Numbers, booleans, null, undefined: literal representation (`42`, `true`, `null`, `undefined`).
   - Arrays: `[ val1, val2, ... ]` with recursive formatting of elements.
   - Objects: `{ key: val, ... }` with recursive formatting of values.
   - Functions: `[Function: name]` or `[Function (anonymous)]`.
   - Symbols: `Symbol(description)`.
   - Circular references: `[Circular]`.
   - Depth limit (4 levels): nested arrays show `[Array]`, nested objects show `[Object]`.
   - Item truncation: after 100 entries, `... N more items`.
6. `fs.readFileSync(path)` reads from the VirtualFs (via callback to `AgentCell`). Returns string (UTF-8).
7. `fs.writeFileSync(path, data)` writes to the VirtualFs (via callback to `AgentCell`). Creates parent dirs implicitly.
8. Uncaught JS exceptions produce an error result with the exception message and stack trace.
9. Infinite loops are bounded by a fuel/interrupt mechanism. Timeout is configurable.

### `process` module

10. `process.env` returns an object of environment variables. The host controls which env vars are visible — this is NOT the real process environment. The agent sees only vars explicitly granted.
11. `process.cwd()` returns the current working directory within the VFS (default: `/workspace`).
12. `process.exit(code)` terminates the JS execution and returns the exit code to the caller. It does NOT terminate the Rust process.

### `fetch` module

Fetch API behavior is specified in S021. The `fetch()` global, `Headers`, `Request`, `Response`, `Blob`, `AbortController`, and `AbortSignal` are registered by `simulacra-fetch::register_globals()` during runtime initialization.

## Assertions

- [x] JS `fs.writeFileSync` then `fs.readFileSync` roundtrip returns identical content.
- [x] `console.log("hello")` captures output to virtual stdout.
- [x] Uncaught exception returns error with message.
- [x] Host function respects VFS path resolution (no escape from virtual root).
- [x] `require()` is not available (throws error). **Tested in `require_is_not_available_and_throws_error`.**
- [x] Infinite loop is interrupted by timeout/fuel mechanism. **Tested in `infinite_loop_is_interrupted_by_timeout`.**
- [x] Host functions for `fs` delegate to `AgentCell` proxy (not directly to VFS). **Tested in `fs_host_functions_delegate_through_agentcell_proxy_instead_of_direct_vfs_spans`. Implemented via `FsProxy` trait (S011).**
- [x] `fs.writeFileSync` and `fs.readFileSync` are registered as host functions (Rust, not polyfills). **Tested in `fs_host_functions_are_native_not_js_polyfills`.**
- [x] `console.log` does not write to real stdout. **Tested in `console_log_does_not_write_to_real_stdout`.**
- [x] JS globals and prototype mutations do not persist across `eval` calls. **Tested in `eval_calls_do_not_share_global_state` and `agent_cell_js_exec_does_not_leak_global_state_across_calls`.**

### `console.log` formatting

- [x] `console.log([1, 2, 3])` outputs `[ 1, 2, 3 ]` (array formatting with spaces).
- [x] `console.log({a: 1, b: 2})` outputs `{ a: 1, b: 2 }` (object formatting).
- [x] `console.log("hello")` outputs `hello` (bare string, no quotes).
- [x] Strings nested inside arrays are single-quoted: `console.log(["a", "b"])` → `[ 'a', 'b' ]`.
- [x] `console.log(null)` outputs `null`.
- [x] `console.log(undefined)` outputs `undefined`.
- [x] `console.log(true)` outputs `true`.
- [x] `console.log(42)` outputs `42`.
- [x] `console.log(3.14)` outputs `3.14`.
- [x] `console.log(function foo() {})` outputs `[Function: foo]`.
- [x] `console.log(() => {})` outputs `[Function (anonymous)]` (arrow functions have empty name).
- [x] Circular references output `[Circular]` instead of crashing.
- [x] Objects nested beyond depth 4 output `[Object]`.
- [x] Arrays nested beyond depth 4 output `[Array]`.
- [x] Multiple arguments are space-separated: `console.log("a", "b", 1)` → `a b 1`.
- [x] `console.log(Object.keys({x:1, y:2}))` outputs `[ 'x', 'y' ]` (array of strings).
- [x] `console.log(Symbol("desc"))` outputs `Symbol(desc)`.
- [x] Arrays with >100 elements truncate with `... N more items`.

### `process` module

- [x] `process.env` returns a host-controlled object, not the real process environment. **Tested in `process_env_returns_host_controlled_object_not_real_env`.**
- [x] `process.cwd()` returns the VFS working directory (default `/workspace`). **Tested in `process_cwd_returns_vfs_working_directory`.**
- [x] `process.exit(0)` terminates JS execution and returns exit code 0 to the Rust caller. **Tested in `process_exit_zero_terminates_js_and_returns_exit_code`.**
- [x] `process.exit(1)` terminates JS execution and returns exit code 1 to the Rust caller. **Tested in `process_exit_one_terminates_js_and_returns_exit_code`.**
- [x] `process.exit()` does NOT terminate the Rust process. **Tested in `process_exit_does_not_terminate_rust_process`.**

### `fetch` module

Fetch API assertions are specified in S021. Legacy assertions below are retained for traceability.

- [x] `fetch("https://allowed.example.com/api")` with matching network capability returns a response. **Tested in `fetch_allowed_url_with_matching_network_capability_returns_a_response`.**
- [x] `fetch("https://denied.example.com/api")` without matching network capability rejects with capability error. **Tested in `fetch_denied_url_without_matching_network_capability_rejects_with_capability_error`.**
- [x] `fetch` response `.json()` parses JSON body. **Tested in `fetch_response_json_parses_the_json_body`.**
- [x] `fetch` response `.text()` returns body as string. **Tested in `fetch_response_text_returns_the_body_as_a_string`.**
- [x] `fetch` response `.status` returns HTTP status code. **Tested in `fetch_response_status_returns_the_http_status_code`.**
- [x] `fetch` dispatches through `AgentCell` proxy, not directly to reqwest. **Tested in `fetch_dispatches_through_agentcell_proxy_instead_of_direct_runtime_network_access`. Implemented via `FetchProxy` trait.**

## Observability (see S010 for conventions)

- [x] JS code execution produces a span with `simulacra.operation.name` = `js_execute` and `simulacra.js.module`.
- [x] Host function calls (fs, fetch) produce child spans under the JS execution span.
- [x] Uncaught exceptions are logged at `ERROR` level with the exception message and stack trace.
