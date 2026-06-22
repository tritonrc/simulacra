# S016 — Native Module Definitions

**Status:** Active
**Crate:** `simulacra-quickjs`

## Problem

Simulacra's `simulacra:` built-in modules are currently implemented as synthetic ESM source strings that wrap global host functions:

```rust
"simulacra:fs" => r#"
    export function readFile(path) { return fs.readFileSync(path); }
    export function writeFile(path, data) { return fs.writeFileSync(path, data); }
"#
```

This causes several issues:
- `Object.keys()` on module namespace objects returns malformed output (raw pointers instead of property names).
- `import * as fs from "simulacra:fs"` does not produce a usable namespace object.
- Adding new exports requires editing fragile JS source strings embedded in Rust.
- Every module import triggers an unnecessary JS parse/compile step.

AWS LLRT solves this with rquickjs's native `ModuleDef` trait, which registers exports directly from Rust. This spec adopts that pattern.

## Behavior

### Native `ModuleDef` registration

1. All `simulacra:` modules (`simulacra:fs`, `simulacra:console`, `simulacra:process`) are implemented as Rust structs that implement rquickjs's `ModuleDef` trait. They are NOT synthetic JS source strings.
2. Each module uses the two-phase pattern: `declare()` registers export names, `evaluate()` populates them with `Func::from()` wrappers pointing to Rust functions.
3. Modules are registered via `Module::declare_def::<FsModule>()` / `Module::evaluate_def::<FsModule>()` in the module loader, not by returning JS source text.

### `simulacra:fs` module

4. Named exports: `readFile(path) -> string`, `writeFile(path, data)`. Same semantics as S003/S014 — delegates through the `AgentCell` proxy for capability/budget/journal enforcement.
5. Default export: an object with `readFile` and `writeFile` as methods, so `import fs from "simulacra:fs"` works.
6. Additional exports beyond S014: `existsSync(path) -> bool`, `mkdirSync(path)`. These delegate through the proxy.

### `simulacra:console` module

7. Named exports: `log(...args)`. Same semantics as S003's `console.log` global.
8. Default export: an object with `log` as a method.

### `simulacra:process` module

9. Named exports: `env` (object), `cwd()` (function), `exit(code)` (function). Same semantics as S003.
10. Default export: an object with `env`, `cwd`, `exit` as properties.

### Module namespace introspection

11. `Object.keys(ns)` on a `simulacra:` module namespace returns the correct export names (e.g., `["readFile", "writeFile", "existsSync", "mkdirSync", "default"]` for `simulacra:fs`).
12. `Object.getOwnPropertyNames(ns)` includes all exports.
13. `typeof ns.readFile === "function"` returns `true` for function exports.

### Import styles

14. `import { readFile } from "simulacra:fs"` — named import works.
15. `import fs from "simulacra:fs"` — default import works, `fs.readFile` is callable.
16. `import * as fs from "simulacra:fs"` — namespace import works, `fs.readFile` is callable, `Object.keys(fs)` returns export names.

### Backward compatibility

17. The `fs`, `process`, and `console` globals from S003 remain functional. Existing code that uses `fs.readFileSync()` continues to work.
18. Existing tests for S003 and S014 continue to pass without modification.
19. The `simulacra:` module loader still handles remote (`https://`) and relative (`./`) imports per S014. Only the built-in module loading mechanism changes.

### `node` shell alias

20. `shell_exec("node script.js")` reads `script.js` from the VFS and executes it through the QuickJS runtime. It returns stdout + result as stdout, and any errors as stderr.
21. `shell_exec("node")` without arguments returns an error with usage instructions.
22. `shell_exec("nodejs script.js")` works identically to `node script.js`.
23. `node` execution routes through `execute_js`, which enforces the full Golden Rule chain (capability, budget, journal, span).

## Assertions

### Native module registration

- [x] `simulacra:fs` module is registered via `ModuleDef`, not as a synthetic JS source string. (simulacra-quickjs `simulacra_fs_module_is_registered_via_moduledef_not_synthetic_source`)
- [x] `simulacra:console` module is registered via `ModuleDef`, not as a synthetic JS source string. (simulacra-quickjs `simulacra_console_module_is_registered_via_moduledef_not_synthetic_source`)
- [x] `simulacra:process` module is registered via `ModuleDef`, not as a synthetic JS source string. (simulacra-quickjs `simulacra_process_module_is_registered_via_moduledef_not_synthetic_source`)

### `simulacra:fs` exports

- [x] `import { readFile } from "simulacra:fs"` returns a function that reads from VFS. (simulacra-quickjs `simulacra_fs_named_read_file_import_reads_from_vfs`)
- [x] `import { writeFile } from "simulacra:fs"` returns a function that writes to VFS. (simulacra-quickjs `simulacra_fs_named_write_file_import_writes_to_vfs`)
- [x] `import { existsSync } from "simulacra:fs"` returns `true` for existing paths, `false` otherwise. (simulacra-quickjs `simulacra_fs_exists_sync_named_export_reports_vfs_presence`)
- [x] `import { mkdirSync } from "simulacra:fs"` creates a directory in VFS. (simulacra-quickjs `simulacra_fs_mkdir_sync_named_export_creates_directory_in_vfs`)
- [x] `import fs from "simulacra:fs"; fs.readFile("/workspace/test.txt")` works via default export. (simulacra-quickjs `simulacra_fs_default_export_exposes_read_and_write_methods`)

### `simulacra:console` exports

- [x] `import { log } from "simulacra:console"` captures output to virtual stdout. (simulacra-quickjs `simulacra_console_named_log_import_captures_stdout`)
- [x] `import console from "simulacra:console"; console.log("hi")` works via default export. (simulacra-quickjs `simulacra_console_default_export_exposes_log_method`)

### `simulacra:process` exports

- [x] `import { env } from "simulacra:process"` returns the host-controlled environment object. (simulacra-quickjs `simulacra_process_named_env_import_returns_host_controlled_environment_object`)
- [x] `import { cwd } from "simulacra:process"` returns `/workspace`. (simulacra-quickjs `simulacra_process_named_cwd_import_returns_workspace`)
- [x] `import { exit } from "simulacra:process"` terminates JS execution with the given code. (simulacra-quickjs `simulacra_process_named_exit_import_terminates_execution_with_the_given_code`)
- [x] `import process from "simulacra:process"; process.cwd()` works via default export. (simulacra-quickjs `simulacra_process_default_export_exposes_env_cwd_and_exit`)

### Module namespace introspection

- [x] `import * as fs from "simulacra:fs"; Object.keys(fs)` returns an array containing `"readFile"` and `"writeFile"`. (simulacra-quickjs `simulacra_fs_namespace_object_keys_list_all_expected_exports`)
- [x] `import * as fs from "simulacra:fs"; typeof fs.readFile` returns `"function"`. (simulacra-quickjs `simulacra_fs_namespace_read_file_export_has_function_type`)
- [x] `Object.keys()` result does not contain raw pointer strings or malformed entries. (simulacra-quickjs `simulacra_fs_namespace_keys_do_not_expose_malformed_pointer_entries`)

### Import styles

- [x] Named imports (`import { readFile } from "simulacra:fs"`) work. (simulacra-quickjs `simulacra_fs_named_import_style_is_supported`)
- [x] Default imports (`import fs from "simulacra:fs"`) work. (simulacra-quickjs `simulacra_fs_default_import_style_is_supported`)
- [x] Namespace imports (`import * as fs from "simulacra:fs"`) work. (simulacra-quickjs `simulacra_fs_namespace_import_style_is_supported`)

### Backward compatibility

- [x] `fs.readFileSync(path)` global still works after native module migration. (simulacra-quickjs `legacy_fs_global_readfilesync_remains_available_after_native_module_migration`)
- [x] `console.log(msg)` global still works after native module migration. (simulacra-quickjs `legacy_console_global_log_remains_available_after_native_module_migration`)
- [x] `process.cwd()` global still works after native module migration. (simulacra-quickjs `legacy_process_global_cwd_remains_available_after_native_module_migration`)
- [x] All existing S003 tests pass without modification. (simulacra-quickjs `s003_compatibility_smoke_test_stays_green_after_native_module_migration`)
- [x] All existing S014 tests pass without modification. (simulacra-quickjs `s014_remote_and_relative_imports_stay_green_after_native_module_migration`)

### `node` shell alias

- [x] `shell_exec("node /workspace/script.js")` executes the script through QuickJS and returns output. (simulacra-sandbox `shell_exec_node_script_executes_through_quickjs_and_returns_output`)
- [x] `shell_exec("node")` without arguments returns exit code 1 with usage error. (simulacra-sandbox `shell_exec_node_without_arguments_returns_usage_error_with_exit_code_one`)
- [x] `shell_exec("nodejs /workspace/script.js")` works identically to the `node` variant. (simulacra-sandbox `shell_exec_nodejs_alias_matches_node_execution`)
- [x] `node` execution enforces capability checks (js must be enabled). (simulacra-sandbox `shell_exec_node_requires_javascript_capability`)
- [x] `node` execution is journaled. (simulacra-sandbox `shell_exec_node_execution_is_journaled`)

## Observability (see S010 for conventions)

- [x] Native module loading does not produce additional spans compared to synthetic module loading (no regression). (simulacra-quickjs `built_in_module_loading_does_not_emit_additional_spans_compared_to_plain_eval`)
- [x] `node` shell alias execution includes the `sandbox_js_exec` and `js_execute` spans (same as `execute_js`), plus `sandbox_shell_exec` and `vfs_read` from the shell/file-read path. (simulacra-sandbox `node_shell_alias_produces_the_same_operation_spans_as_execute_js`)
