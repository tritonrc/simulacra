# S014 — ESM Module Support

**Status:** Draft
**Crate:** `simulacra-quickjs`

## Behavior

### Module Resolution

1. The QuickJS runtime uses a custom module resolver that distinguishes three specifier classes by prefix:
   - **`simulacra:`** prefix — local standard library modules provided by the host (e.g., `import { readFile } from "simulacra:fs"`).
   - **`http://` or `https://`** prefix — remote modules fetched over the network (e.g., `import lodash from "https://esm.sh/lodash"`).
   - **Node-like built-in aliases** (`fs`, `console`, `process`, `path`, `crypto`) — resolved to the corresponding `simulacra:` module.
   - **Other bare specifiers** — rejected with a clear error: `"Bare specifier 'foo' is not allowed. Built-in module aliases are available for: fs, console, process, path, crypto. Use 'simulacra:' for built-in modules or 'http(s)://' for remote modules."`.
2. `require()` remains unavailable. Attempting `require()` throws an error (unchanged from S003).
3. Relative specifiers (`./foo.js`, `../bar.js`) resolve relative to the importing module's path within the VFS. Only VFS-resident modules may use relative imports.

### Simulacra Standard Library (`simulacra:` modules)

4. Simulacra-provided modules are registered at runtime startup as synthetic ESM modules backed by Rust host functions. They are NOT loaded from VFS files — they are compiled into the runtime.
5. Initial `simulacra:` modules:
   - `simulacra:fs` — exports `readFile(path)`, `writeFile(path, data)`, `readFileSync(path)`, `writeFileSync(path, data)`, `existsSync(path)`, `mkdirSync(path)`, `readdirSync(path)`, `statSync(path)`, `unlinkSync(path)`, `renameSync(oldPath, newPath)`, and `appendFileSync(path, data)`. These delegate through the `AgentCell` proxy.
   - `simulacra:console` — exports `log(...args)`, `error(...args)`, `warn(...args)`, `info(...args)`, and `debug(...args)`. Delegates to the same virtual stdout capture as the `console` global.
   - `simulacra:process` — exports `env`, `cwd()`, `exit(code)`. Same semantics as S003's `process` global.
   - `simulacra:path` — exports path helpers backed by Rust host functions.
   - `simulacra:crypto` — exports supported crypto helpers backed by Rust host functions.
6. The existing `fs`, `process`, and `console` globals (S003) remain available as legacy convenience for simple scripts. The `simulacra:` modules are the canonical API. Built-in bare aliases are compatibility imports for these same modules, not package resolution. New host APIs (e.g., `simulacra:http`) are added ONLY as `simulacra:` modules — never as new globals.
7. Importing an unknown `simulacra:` module (e.g., `simulacra:nonexistent`) produces an error: `"Unknown simulacra module: 'nonexistent'. Available: fs, console, process, path, crypto."`.

### Remote Modules (`http://` / `https://`)

8. Remote module imports are fetched via HTTP through the `AgentCell` proxy. The proxy enforces the full Golden Rule chain: capability check (`network` permission against the URL) -> budget check -> journal write (`JournalEntryKind::HttpRequest`) -> OTel span -> fetch -> return source text.
9. If the `network` capability denies the URL, the import fails with a capability error: `"Network access denied for module URL: 'https://esm.sh/lodash'."`. The module does not load.
10. If the HTTP fetch fails (network error, non-2xx status, timeout), the import fails with an error that includes the URL and the failure reason: `"Failed to fetch module 'https://esm.sh/lodash': 404 Not Found."`.
11. Remote modules are expected to return valid ESM JavaScript in the response body. The `Content-Type` header is not enforced (CDNs vary), but the response body must parse as a valid ES module.
12. Remote modules may themselves contain `import` statements. Transitive imports follow the same resolution rules (simulacra:/https:/relative) and each goes through the proxy independently.

### Module Caching

13. Fetched remote module source is cached in a per-`JsRuntime` source cache keyed by the fully resolved URL. A second `import` of the same URL within the same `JsRuntime` does NOT re-fetch — it recompiles from cached source in that eval's fresh QuickJS context.
14. The source cache lives for the lifetime of the `JsRuntime` wrapper (one per `AgentCell`). There is no cross-agent cache sharing. There is no disk cache.
15. Cache entries store raw source text, not compiled module instances. Re-importing a cached module does not preserve JS global state, module singleton state, or top-level side effects across tool calls.
16. `simulacra:` modules are always available and never fetched — they are not subject to caching because they are built-in.

### Security

17. Remote module code executes in the same QuickJS sandbox as agent code. It has the same fuel/interrupt timeout limits (S003, behavior 9). Remote modules do not gain additional privileges.
18. Remote module code that calls `simulacra:fs` or uses the `fs` global operates through the same `AgentCell` proxy with the same capability token. A remote module cannot bypass capability restrictions.
19. A remote module that attempts to `import` from another URL is subject to the same `network` capability check. An agent with `network: ["https://esm.sh/**"]` can import from esm.sh, but a transitive import to `https://evil.example.com/payload.js` will be denied if the capability does not cover it.

### Error Handling

20. All module resolution errors are surfaced as uncaught exceptions in the QuickJS runtime, producing a `JsError::Execution` with the error message and (where available) a stack trace.
21. If a remote fetch times out (governed by the runtime's HTTP timeout, not the JS fuel timeout), the error message includes the URL and indicates a timeout.
22. Circular imports between remote modules follow standard ESM semantics (partially initialized module bindings). The runtime does not add special handling beyond what QuickJS provides.

## Assertions

### Module resolution

- [x] `import { readFile } from "simulacra:fs"` successfully imports the built-in fs module. **Tested in `simulacra_fs_module_can_be_imported`.**
- [x] `import { log } from "simulacra:console"` successfully imports the built-in console module. **Tested in `simulacra_console_module_can_be_imported`.**
- [x] `import { env, cwd, exit } from "simulacra:process"` successfully imports the built-in process module. **Tested in `simulacra_process_module_can_be_imported`.**
- [x] `import foo from "bare-specifier"` throws an error with message indicating bare specifiers are not allowed. **Tested in `bare_specifier_imports_are_rejected_with_a_clear_error`.**
- [x] `require("simulacra:fs")` throws an error (require remains unavailable). **Tested in `require_remains_unavailable_for_simulacra_modules`.**
- [x] `import { readFile } from "simulacra:fs"; const data = readFile("/workspace/test.txt");` reads from VFS via AgentCell proxy. **Tested in `simulacra_fs_read_file_reads_from_vfs_via_host_proxy`.**
- [x] `import { writeFile } from "simulacra:fs"; writeFile("/workspace/out.txt", "hello");` writes to VFS via AgentCell proxy. **Tested in `simulacra_fs_write_file_writes_to_vfs_via_host_proxy`.**
- [x] `import { noSuchExport } from "simulacra:fs"` produces an error about the missing export. **Tested in `missing_simulacra_module_exports_surface_module_errors`.**
- [x] `import x from "simulacra:nonexistent"` throws an error listing available simulacra modules. **Tested in `unknown_simulacra_modules_list_available_modules`.**

### Remote modules

- [x] `import _ from "https://esm.sh/lodash"` with matching `network` capability fetches and loads the module. **Tested in `remote_module_imports_fetch_and_load_when_network_capability_allows`.**
- [x] `import _ from "https://esm.sh/lodash"` without matching `network` capability fails with a capability error. **Tested in `remote_module_imports_fail_with_capability_error_when_url_is_denied`.**
- [x] Remote module fetch goes through the `AgentCell` proxy (capability check, budget check, journal write, span emission all occur). **Tested in `remote_module_fetch_uses_agent_cell_proxy_for_budget_journal_and_span_emission` (s014_esm_red).**
- [x] Remote module fetch failure (e.g., 404) produces an error with the URL and HTTP status. **Tested in `remote_module_http_failures_include_the_url_and_status`.**
- [x] Remote module fetch network error produces an error with the URL and failure reason. **Tested in `remote_module_network_errors_include_the_url_and_reason`.**

### Caching

- [x] Importing the same remote URL twice within one `JsRuntime` wrapper triggers only one HTTP fetch while evaluating in a fresh JS context each time. **Tested in `importing_the_same_remote_url_twice_uses_the_runtime_cache`.**
- [x] Two different `JsRuntime` instances do not share module cache (each fetches independently). **Tested in `separate_runtimes_do_not_share_the_remote_module_cache`.**

### Relative imports

- [x] A VFS-resident module at `/workspace/lib/utils.js` can `import { helper } from "./helper.js"` which resolves to `/workspace/lib/helper.js` in the VFS. **Tested in `vfs_modules_can_resolve_relative_imports_within_the_virtual_filesystem`.**
- [x] A relative import from a remote module resolves relative to the remote URL (e.g., `./util.js` from `https://esm.sh/pkg/index.js` resolves to `https://esm.sh/pkg/util.js`). **Tested in `remote_modules_resolve_relative_imports_against_their_url`.**

### Security

- [x] Remote module code is subject to the same fuel/interrupt timeout as inline agent code. **Tested in `remote_module_code_uses_the_same_execution_timeout_as_inline_code`.**
- [x] Remote module code that calls `simulacra:fs` operations goes through capability checks (denied paths are denied). **Tested in `remote_module_code_calling_simulacra_fs_still_hits_agent_cell_capability_checks` (s014_esm_red).**
- [x] Transitive remote import to a URL not covered by the agent's `network` capability is denied. **Tested in `transitive_remote_imports_are_checked_against_network_capabilities`.**

### Backward compatibility

- [x] The `fs`, `process`, and `console` globals from S003 remain functional alongside `simulacra:` module imports. **Tested in `legacy_globals_remain_usable_alongside_simulacra_module_imports`.**
- [x] Code that does not use `import` statements continues to work as before (S003 behavior is preserved). **Tested in `legacy_scripts_continue_to_work_after_module_loading_is_enabled`.**

## Observability (see S010 for conventions)

- [x] Remote module fetch produces a child span under the JS execution span with `simulacra.operation.name` = `module_fetch` and `simulacra.module.url` = the requested URL. **Tested in `remote_module_fetch_creates_a_child_span_with_module_url`.**
- [x] Remote module cache hit emits a span event (not a full span) with `simulacra.module.cache` = `hit` and the URL. **Tested in `remote_module_cache_hits_emit_a_span_event_with_hit_metadata`.**
- [x] Module resolution failure (any class) is logged at `ERROR` with the specifier and the failure reason. **Tested in `module_resolution_failures_are_logged_at_error_with_specifier_and_reason`.**
- [x] `simulacra.module.fetches` counter is incremented for each remote module HTTP request (cache misses only). **Tested in `remote_module_fetches_increment_the_fetch_counter_on_cache_miss_only`.**
- [x] Capability denial for a remote module URL emits a `WARN`-level event with `simulacra.capability.operation` = `module_fetch` and `simulacra.capability.reason`. **Tested in `remote_module_url_capability_denials_emit_warn_events_with_module_fetch_metadata` (s014_esm_red).**
