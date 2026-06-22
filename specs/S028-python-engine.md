# S028 — Monty Python Engine

**Status:** Active
**Crate:** `simulacra-python` (new)

## Dependencies

- **S006** — Resource budgets (Monty resource limits mapped to `ResourceBudget`)
- **S010** — Observability conventions
- **S011** — Sandbox composition (Golden Rule enforcement via `AgentCell` proxy)
- **S012** — Built-in tools (`PyExecTool` implements same `Tool` trait as `JsExecTool`)

## Scope

Add `py_exec` as a built-in tool powered by Monty — a Rust-native Python interpreter — with sandboxed execution, external function mediation through the Golden Rule, and resource limit enforcement.

**In scope:**
- New `simulacra-python` crate wrapping Monty (pydantic/monty, git dependency)
- `PythonRuntime` — creates `MontyRun`, configures resource limits
- `PyExecTool` — `Tool` trait implementation for `py_exec` builtin
- External functions mapped to Simulacra operations: `read_file`, `write_file`, `list_dir`, `http_get`, `http_post`, `env`
- External function calls mediated by `AgentCell` proxy (Golden Rule: capability, budget, journal, OTel)
- Monty resource limits (memory, allocation count, stack depth, execution time) mapped to `ResourceBudget`
- Feature-gated: `features = ["python"]` in `simulacra-cli`
- Capability token: `python = true` enables `py_exec`

**Out of scope:**
- Third-party Python packages (no pip, no `import numpy`)
- Full CPython compatibility (Monty has no classes yet, limited stdlib)
- Python REPL / interactive mode
- Python-based tool hosting (WASM is the tool hosting layer — S025)
- Python-based governance hooks (JS is the hook runtime — S026)
- Serialized execution state (Monty supports `dump`/`load` but not needed in S028)

## Context

Agents currently execute computation via `js_exec` (QuickJS). Adding Python gives agents access to a language with broader model training data coverage, natural data science idioms, and syntax most LLMs are deeply fluent in. Many agent tasks — CSV parsing, JSON transformation, statistical computation, string processing — are more naturally expressed in Python.

Monty is purpose-built for this use case: a Rust-native Python interpreter from the Pydantic team, designed for executing AI-generated Python. Its pause/resume model maps perfectly to Simulacra's architecture — when Python calls an external function (e.g., `read_file`), Monty yields control to the host, Simulacra dispatches through `AgentCell` for Golden Rule enforcement, and the result flows back to Python. No FFI boundary, no CPython embedding, no GIL.

Key Monty properties that align with Simulacra:
- **0.06ms startup** — no cold start penalty per tool call
- **Pause/resume** — external function calls yield to host, enabling Golden Rule mediation
- **Built-in resource limits** — memory, allocations, stack depth, execution time
- **Supported stdlib** — `sys`, `os`, `typing`, `asyncio`, `re`, `datetime`, `json` cover most agent computation needs
- **Pure Rust** — no system Python dependency, cross-compiles cleanly
- **MIT license** — compatible with Simulacra's licensing

The crate is feature-gated because Monty is a git dependency (not yet on crates.io) and adds compile time. Default builds exclude it.

## Design

### PythonRuntime

Owns a configured Monty interpreter instance with resource limits:

```rust
pub struct PythonRuntime {
    resource_limits: PythonResourceLimits,
    external_fns: Vec<ExternalFunctionDef>,
}

pub struct PythonResourceLimits {
    pub max_memory_bytes: u64,      // 0 = unlimited
    pub max_allocations: u64,       // 0 = unlimited
    pub max_stack_depth: u32,       // 0 = unlimited
    pub max_execution_time_ms: u64, // 0 = unlimited
}
```

- `PythonRuntime::new(limits, external_fns)` creates a runtime ready to execute code.
- Each `execute(code)` call creates a fresh `MontyRun` — no state persists between calls.
- External functions are registered with Monty before execution begins.
- When Python calls an external function, Monty yields a `PauseReason::ExternalCall` with the function name and arguments. The host resolves the call and resumes execution with the result.

### PyExecTool

Implements the `Tool` trait. Same interface as `JsExecTool`:

```rust
pub struct PyExecTool {
    runtime: PythonRuntime,
}
```

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "code": {
      "type": "string",
      "description": "Python code to execute"
    }
  },
  "required": ["code"]
}
```

Each `call()` invocation:
1. Parse `code` from arguments
2. Create fresh `MontyRun` with resource limits and external functions
3. Execute code. On each `PauseReason::ExternalCall`:
   a. Map function name to Simulacra operation
   b. Dispatch through `AgentCell` proxy (capability check, budget deduction, journal entry, OTel span)
   c. Resume `MontyRun` with the result
4. Collect stdout output (from `print()` calls)
5. Return output as `ToolResult`
6. On resource limit exceeded, return `ToolError::ExecutionFailed` with descriptive message
7. On Python exception, return `ToolError::ExecutionFailed` with exception message and traceback

### External function mapping

Python code calls these functions. Each call pauses Monty and dispatches through `AgentCell`:

| Python function | Simulacra operation | AgentCell method |
|---|---|---|
| `read_file(path: str) -> str` | VFS read | `AgentCell::read_file(path)` |
| `write_file(path: str, content: str) -> None` | VFS write | `AgentCell::write_file(path, content)` |
| `list_dir(path: str) -> list[str]` | VFS list | `AgentCell::list_dir(path)` |
| `http_get(url: str) -> str` | HTTP GET | `AgentCell::http_get(url)` |
| `http_post(url: str, body: str) -> str` | HTTP POST | `AgentCell::http_post(url, body)` |
| `env(name: str) -> str | None` | Env read | `AgentCell::env(name)` |

**Golden Rule delegation:** `simulacra-python` itself does NOT check capabilities, budgets, or write journal entries. It is a pure execution environment. All Golden Rule enforcement happens at the `AgentCell` proxy layer (same as `simulacra-quickjs` — see S011). External functions that perform side effects call back through `AgentCell` proxy methods, which enforce the full chain.

### print() capture

`print()` output is captured to a buffer — not written to real stdout. The buffer contents become the tool result. This mirrors `console.log` capture in QuickJS (S003).

Multiple `print()` calls produce newline-separated output. `print("a"); print("b")` returns `"a\nb\n"`.

### Resource limit mapping

Monty's built-in resource limits map to Simulacra's `ResourceBudget`:

| Monty limit | ResourceBudget field | Behavior on exceeded |
|---|---|---|
| Memory bytes | `max_python_memory` | `ToolError::ExecutionFailed("memory limit exceeded")` |
| Allocation count | `max_python_allocations` | `ToolError::ExecutionFailed("allocation limit exceeded")` |
| Stack depth | `max_python_stack_depth` | `ToolError::ExecutionFailed("stack depth exceeded")` |
| Execution time | `max_python_time_ms` | `ToolError::ExecutionFailed("execution time exceeded")` |

Defaults when not configured: Monty's own defaults (sensible for sandboxed AI code). Zero means unlimited, consistent with all other budget fields.

### Feature flag

`simulacra-python` is an optional dependency of `simulacra-cli`:

```toml
# simulacra-cli/Cargo.toml
[features]
default = []
python = ["dep:simulacra-python"]
```

CLI bootstrap wraps Python tool registration in `#[cfg(feature = "python")]`. Default builds don't include Monty.

```toml
# simulacra-python/Cargo.toml
[dependencies]
monty = { git = "https://github.com/pydantic/monty" }
simulacra-types = { path = "../simulacra-types" }
```

### Capability token

`py_exec` requires `python = true` in the agent's capability token:

```toml
[agent_types.default.capabilities]
python = true
```

When `python = false` (or absent), `py_exec` calls return `ToolError::CapabilityDenied`. This is enforced by `AgentCell`, not by `PyExecTool` itself.

### Crate position in dependency graph

```
simulacra-types (leaf)
  ├→ simulacra-python (monty, simulacra-types)
  ├→ simulacra-quickjs (rquickjs, simulacra-types)
  ├→ simulacra-wasm (wasmtime, simulacra-types)
  └→ ...
       └→ simulacra-cli (optional: simulacra-python, simulacra-quickjs, simulacra-wasm)
```

`simulacra-python` depends only on `simulacra-types` (for `ToolDefinition`, `ToolError`, `Tool` trait). It does not depend on `simulacra-quickjs`, `simulacra-tool`, `simulacra-sandbox`, or any other Simulacra crate.

## Behavior

### Runtime lifecycle

1. `PythonRuntime::new(limits, external_fns)` creates a runtime with configured resource limits and external function definitions.
2. Each `execute(code)` creates a fresh `MontyRun`. No state persists between calls.
3. The `MontyRun` is dropped after execution completes or errors. No cleanup required.

### Tool execution

4. `PyExecTool::call()` parses the `code` argument from JSON input.
5. Missing or empty `code` argument returns `ToolError::InvalidArguments`.
6. Code is executed in a fresh `MontyRun` with all configured external functions available.
7. `print()` output is captured to a buffer. The buffer contents become the `ToolResult`.
8. If code produces no output (no `print()` calls, no return value), the result is an empty string.
9. Python exceptions return `ToolError::ExecutionFailed` with the exception type, message, and traceback.
10. Syntax errors return `ToolError::ExecutionFailed` with the parse error and line number.

### External function mediation

11. When Python calls an external function, Monty pauses and yields `PauseReason::ExternalCall` with the function name and serialized arguments.
12. The host maps the function name to a Simulacra operation and dispatches through `AgentCell`.
13. `AgentCell` enforces the Golden Rule: capability check → budget check → execute → journal entry → OTel span.
14. The result (or error) is serialized back to Python and execution resumes.
15. If the capability check fails, the external function raises a Python `PermissionError` with the denial reason.
16. If the budget is exhausted, the external function raises a Python `RuntimeError` with the budget exhaustion message.
17. Calling an unregistered external function raises a Python `NameError`.

### Resource limits

18. Memory limit exceeded → Monty terminates execution → `ToolError::ExecutionFailed("memory limit exceeded: {bytes} bytes")`.
19. Allocation limit exceeded → Monty terminates execution → `ToolError::ExecutionFailed("allocation limit exceeded: {count} allocations")`.
20. Stack depth exceeded → Monty terminates execution → `ToolError::ExecutionFailed("stack depth exceeded")`.
21. Execution time exceeded → Monty terminates execution → `ToolError::ExecutionFailed("execution time exceeded: {ms}ms")`.
22. Resource limits are per-invocation. Each `execute()` call starts with fresh counters.

### Sandbox properties

23. Python code cannot access the host filesystem directly. All file access goes through `read_file`/`write_file`/`list_dir` external functions or Monty's mediated `pathlib.Path` operations (`read_text`, `write_text`, `iterdir`, `exists`, `is_file`, `is_dir`), which are mediated by `AgentCell`.
24. Python code cannot make network requests directly. All HTTP goes through `http_get`/`http_post` external functions, which are mediated by `AgentCell`.
25. Python code cannot access host environment variables directly. `env()` external function returns only vars granted by capability token.
26. Python code cannot import third-party packages. Only Monty's supported stdlib is available: `sys`, `os`, `typing`, `asyncio`, `re`, `datetime`, `json`.
27. Python code cannot spawn processes, open sockets, or perform any I/O except through registered external functions.
28. No state persists between `py_exec` calls. Each invocation is fully isolated.

### Monty stdlib behavior

29. `import json` works — `json.loads()`, `json.dumps()` available for data processing.
30. `import re` works — regex operations available.
31. `import datetime` works — date/time computation available.
32. `import os` provides limited functionality (no actual OS access — Monty's sandbox).
33. `import typing` works — type annotations available (no runtime enforcement).
34. Importing unsupported modules raises `ModuleNotFoundError`.

### CLI integration

35. Python tool registration is behind `#[cfg(feature = "python")]` in the CLI bootstrap.
36. Default builds (`cargo install simulacra`) do not include Monty.
37. `cargo install simulacra --features python` includes Python execution support.
38. When the `python` feature is enabled and the agent's capability token includes `python = true`, `py_exec` is registered in `ToolRegistry` alongside other builtins.
39. When the feature is not enabled, `py_exec` is not available regardless of capability token.

## Assertions

### Tool execution

- [x] `py_exec` with `code: "print('hello')"` returns `"hello\n"`.
- [x] `py_exec` with `code: "print(2 + 2)"` returns `"4\n"`.
- [x] `py_exec` with `code: "x = 42\nprint(x)"` returns `"42\n"`.
- [x] `py_exec` with empty `code` returns `ToolError::InvalidArguments`.
- [x] `py_exec` with missing `code` field returns `ToolError::InvalidArguments`.
- [x] `py_exec` with syntax error returns `ToolError::ExecutionFailed` containing the line number.
- [x] `py_exec` with uncaught exception returns `ToolError::ExecutionFailed` with exception type and message.
- [x] Multiple `print()` calls produce newline-separated output.
- [x] No state persists between successive `py_exec` calls (variable from call 1 is not visible in call 2).
- [x] `PyExecTool` implements the `Tool` trait and works through `ToolRegistry`.

### External function mediation

- [x] `read_file("path")` in Python dispatches through `AgentCell::read_file` and returns file content.
- [x] `write_file("path", "content")` in Python dispatches through `AgentCell::write_file`.
- [x] `list_dir("path")` in Python dispatches through `AgentCell::list_dir` and returns a list of strings.
- [x] `pathlib.Path(path).read_text()` dispatches through the mediated read operation.
- [x] `pathlib.Path(path).write_text(content)` dispatches through the mediated write operation.
- [x] `pathlib.Path(path).iterdir()` dispatches through the mediated directory listing operation.
- [x] `pathlib.Path(path).exists()`, `.is_file()`, and `.is_dir()` dispatch through mediated file/directory operations.
- [x] `http_get("url")` in Python dispatches through `AgentCell::http_get` and returns response body.
- [x] `http_post("url", "body")` in Python dispatches through `AgentCell::http_post` and returns response body.
- [x] `env("NAME")` in Python dispatches through `AgentCell::env` and returns the value or `None`.
- [x] External function with denied capability raises `PermissionError` in Python.
- [x] External function with exhausted budget raises `RuntimeError` in Python.
- [x] Calling an unregistered function raises `NameError` in Python.

### Resource limits

- [x] Execution exceeding memory limit returns `ToolError::ExecutionFailed` with "memory limit exceeded".
- [x] Execution exceeding allocation limit returns `ToolError::ExecutionFailed` with "allocation limit exceeded".
- [x] Execution exceeding stack depth returns `ToolError::ExecutionFailed` with "stack depth exceeded".
- [x] Execution exceeding time limit returns `ToolError::ExecutionFailed` with "execution time exceeded".
- [x] Resource limit of 0 is treated as unlimited.
- [x] Resource counters reset between invocations.
- [x] Infinite loop (`while True: pass`) is terminated by execution time limit.

### Sandbox isolation

- [x] Python code cannot read host filesystem without `read_file` external function.
- [x] Python code cannot write host filesystem without `write_file` external function.
- [x] Python code cannot make HTTP requests without `http_get`/`http_post` external functions.
- [x] Python `os.environ` does not expose host environment variables.
- [x] `import subprocess` raises `ModuleNotFoundError`.
- [x] `import socket` raises `ModuleNotFoundError`.
- [x] `import ctypes` raises `ModuleNotFoundError`.

### Monty stdlib

- [x] `import json; print(json.dumps({"a": 1}))` succeeds and returns valid JSON.
- [x] `import re; print(re.match(r"\d+", "42").group())` succeeds and returns `"42"`.
- [x] `import datetime; print(datetime.date.today())` succeeds and returns a date string.
- [x] Importing an unsupported module raises `ModuleNotFoundError`.

### CLI integration

- [x] With `features = ["python"]` and `python = true` capability, `py_exec` is registered in `ToolRegistry`.
- [x] Without the feature flag, `py_exec` is not available.
- [x] With the feature flag but `python = false` capability, `py_exec` calls return capability denied.

## Observability (see S010)

- [x] `simulacra_py_exec` span wraps each `py_exec` invocation with `simulacra.python.code_length`, `simulacra.python.output_length`, `simulacra.python.duration_ms`.
- [x] `simulacra_py_external_call` span wraps each external function call with `simulacra.python.function`, `simulacra.python.function_duration_ms`.
- [x] `simulacra.python.executions` counter incremented per `py_exec` call with `status` label (`success`|`error`).
- [x] `simulacra.python.external_calls` counter incremented per external function call with `function` label.
- [x] `simulacra.python.execution_time_ms` histogram records execution time per call.
- [x] `simulacra.python.resource_limit_exceeded` counter incremented on resource limit hit with `limit_type` label (`memory`|`allocations`|`stack_depth`|`time`).
- [x] `tracing::info!` on successful `py_exec` completion with output length.
- [x] `tracing::warn!` on resource limit exceeded, capability denied, budget exhausted.
- [x] `tracing::error!` on Python exception with traceback.
