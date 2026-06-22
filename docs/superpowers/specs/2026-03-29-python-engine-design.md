# S028 — Monty Python Engine

**Status:** Active
**Crates involved:** `simulacra-python` (new), `simulacra-cli`

## Dependencies

- **S006** — Resource budgets (resource limit mapping)
- **S010** — Observability conventions
- **S011** — Sandbox composition (Golden Rule enforcement)
- **S012** — Built-in tools (same `Tool` trait)

## Scope

Add `py_exec` builtin tool powered by Monty (pydantic/monty) — a Rust-native Python interpreter with pause/resume external function model, built-in resource limits, and sandboxed execution. Feature-gated behind `python`.

**In scope:** `simulacra-python` crate, `PythonRuntime`/`PyExecTool`, external function mapping (fs, http, env), resource limits, `python` capability token, CLI feature gate.

**Out of scope:** Third-party packages, CPython compatibility, Python REPL, Python-based tools/hooks, serialized execution state.

Full spec: `specs/S028-python-engine.md`

## Context

Agents use `js_exec` for computation. Python adds broader model fluency — LLMs produce more natural Python for data science, string processing, and statistical computation. Monty is purpose-built for AI-generated Python execution: pure Rust, 0.06ms startup, pause/resume for host function calls, built-in sandboxing.

## Design

### Architecture

```
simulacra-types (leaf)
  ├→ simulacra-python (monty, simulacra-types)
  ├→ simulacra-quickjs (rquickjs, simulacra-types)
  ├→ simulacra-wasm (wasmtime, simulacra-types)
  └→ ...
       └→ simulacra-cli (optional: simulacra-python via features=["python"])
```

`simulacra-python` depends only on `simulacra-types`. Same leaf position as `simulacra-quickjs` and `simulacra-wasm`.

### Execution Model

```
  LLM response: tool_use("py_exec", {"code": "print(read_file('/workspace/data.csv'))"})
       │
       ▼
  PyExecTool::call(args)
       │  parse code from args
       │  create fresh MontyRun with resource limits + external functions
       ▼
  MontyRun::execute(code)
       │
       │  ── Python calls read_file("/workspace/data.csv") ──
       │
       ▼  PauseReason::ExternalCall("read_file", ["/workspace/data.csv"])
  Host dispatches through AgentCell::read_file("/workspace/data.csv")
       │  → capability check
       │  → budget check
       │  → VFS read
       │  → journal entry
       │  → OTel span
       ▼
  MontyRun::resume(file_contents)
       │
       │  ── Python calls print() ──
       │
       ▼  Output captured to buffer
  ToolResult { content: "<file contents>\n", is_error: false }
```

Key insight: Monty's pause/resume model means external function calls are **synchronous from Python's perspective** but **mediated by Simulacra from the host's perspective**. Every I/O operation goes through AgentCell. The Python code has no way to bypass the Golden Rule.

### External Functions

Six external functions cover the operations agents need:

```python
# File operations (via AgentCell → VFS)
content: str = read_file("/workspace/data.csv")
write_file("/workspace/output.json", json.dumps(result))
entries: list[str] = list_dir("/workspace")

# HTTP operations (via AgentCell → simulacra-http)
response: str = http_get("https://api.example.com/data")
response: str = http_post("https://api.example.com/submit", json.dumps(payload))

# Environment (via AgentCell → filtered env)
token: str | None = env("API_TOKEN")
```

All six pause Monty, dispatch through AgentCell, and resume with results. Errors from AgentCell (capability denied, budget exhausted) are translated to Python exceptions.

### Resource Limits

Monty provides four resource limit knobs, all enforced inside the interpreter:

| Limit | Monty API | Default | Behavior |
|---|---|---|---|
| Memory | `max_memory_bytes` | Monty default | Terminates execution |
| Allocations | `max_allocations` | Monty default | Terminates execution |
| Stack depth | `max_stack_depth` | Monty default | Terminates execution |
| Execution time | `max_execution_time_ms` | Monty default | Terminates execution |

All limits are per-invocation. Fresh counters on each `py_exec` call. Zero means unlimited, consistent with all Simulacra budget fields.

### Sandbox Properties

The sandbox is enforced by construction, not policy:

1. **No direct filesystem access** — Monty doesn't provide `open()` that maps to host FS. File I/O only through `read_file`/`write_file`/`list_dir` external functions.
2. **No direct network access** — No `socket`, `urllib`, `http.client`. HTTP only through `http_get`/`http_post` external functions.
3. **No process spawning** — `subprocess`, `os.system`, `os.popen` are not available.
4. **No FFI** — `ctypes`, `cffi` not available.
5. **Limited stdlib** — Only `sys`, `os` (limited), `typing`, `asyncio`, `re`, `datetime`, `json`.
6. **No state persistence** — Fresh `MontyRun` per call.

### Feature Gate

```toml
# simulacra-cli/Cargo.toml
[features]
python = ["dep:simulacra-python"]

# simulacra-python/Cargo.toml
[dependencies]
monty = { git = "https://github.com/pydantic/monty" }
simulacra-types = { path = "../simulacra-types" }
```

Default builds exclude Monty. `--features python` opts in.

### Capability Token

```toml
[agent_types.default.capabilities]
python = true  # enables py_exec
```

Enforced by AgentCell, not by PyExecTool. Same pattern as `js_exec` with `javascript` capability.
