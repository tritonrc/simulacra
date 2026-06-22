# S028 Monty Python Engine -- Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `py_exec` as a built-in tool powered by Monty (pydantic/monty) -- a Rust-native Python interpreter -- with sandboxed execution, external function mediation through the Golden Rule, and resource limit enforcement. Feature-gated behind `python`.

**Architecture:** New `simulacra-python` crate wraps Monty. `PythonRuntime` manages `MontyRun` creation and resource limits. `PyExecTool` implements the `Tool` trait. External functions (fs, http, env) use Monty's pause/resume model (`RunProgress::FunctionCall` / `RunProgress::OsCall`) to dispatch through `AgentCell` for Golden Rule enforcement. Feature-gated behind `python` in simulacra-cli.

**Tech Stack:** Rust, monty (git dep from pydantic/monty), serde_json, tracing, opentelemetry

---

## Monty API Reference (verified from source)

The spec describes a simplified API. The **actual** Monty API differs in important ways. This section documents the real API so implementers do not have to guess.

### Key types

```rust
// Core execution
MontyRun::new(code: String, script_name: &str, input_names: Vec<String>) -> Result<Self, MontyException>
MontyRun::start<T: ResourceTracker>(self, inputs: Vec<MontyObject>, resource_tracker: T, print: PrintWriter<'_>) -> Result<RunProgress<T>, MontyException>
MontyRun::run(&self, inputs: Vec<MontyObject>, resource_tracker: impl ResourceTracker, print: PrintWriter<'_>) -> Result<MontyObject, MontyException>

// Pause/resume cycle
enum RunProgress<T: ResourceTracker> {
    FunctionCall(FunctionCall<T>),  // external function call
    OsCall(OsCall<T>),             // OS-level operation (Path.read_text, os.getenv, etc.)
    ResolveFutures(ResolveFutures<T>),
    NameLookup(NameLookup<T>),
    Complete(MontyObject),
}

// FunctionCall fields: function_name: String, args: Vec<MontyObject>, kwargs, call_id, method_call
FunctionCall::resume(self, result: impl Into<ExtFunctionResult>, print: PrintWriter<'_>) -> Result<RunProgress<T>, MontyException>

// OsCall fields: function: OsFunction, args: Vec<MontyObject>, kwargs, call_id
OsCall::resume(self, result: impl Into<ExtFunctionResult>, print: PrintWriter<'_>) -> Result<RunProgress<T>, MontyException>

// External function results
enum ExtFunctionResult {
    Return(MontyObject),
    Error(MontyException),
    Future(u32),
    NotFound(String),
}

// Resource limits
struct ResourceLimits {
    pub max_allocations: Option<usize>,
    pub max_duration: Option<Duration>,
    pub max_memory: Option<usize>,
    pub gc_interval: Option<usize>,
    pub max_recursion_depth: Option<usize>,
}
LimitedTracker::new(limits: ResourceLimits) -> Self
NoLimitTracker  // implements ResourceTracker with no limits

// Output capture
enum PrintWriter<'a> {
    Disabled,
    Stdout,
    Collect(&'a mut String),
    Callback(&'a mut dyn PrintWriterCallback),
}
PrintWriter::reborrow(&mut self) -> PrintWriter<'_>  // critical for loops

// OS functions (enum OsFunction)
// Path: Exists, IsFile, IsDir, ReadText, ReadBytes, WriteText, WriteBytes, Mkdir, Iterdir, Stat, ...
// os: Getenv, GetEnviron
// datetime: DateToday, DateTimeNow

// Exceptions
MontyException::new(exc_type: ExcType, message: Option<String>) -> Self
MontyException::exc_type(&self) -> ExcType
MontyException::message(&self) -> Option<&str>
MontyException::traceback(&self) -> &[StackFrame]
// ExcType variants include: NameError, PermissionError, RuntimeError, MemoryError, TimeoutError, RecursionError, ...

// MontyObject variants: None, Bool(bool), Int(i64), BigInt(BigInt), Float(f64), String(String),
//   Bytes(Vec<u8>), List(Vec<Self>), Tuple(Vec<Self>), Dict(DictPairs), ...
```

### Critical API differences from spec

1. **No `PauseReason::ExternalCall`** -- Monty uses `RunProgress::FunctionCall` for user-registered external functions and `RunProgress::OsCall` for OS-level operations (file I/O, env vars). The spec conflates these.

2. **External functions are resolved via `NameLookup`** -- When Python code calls `read_file(path)`, Monty first yields `RunProgress::NameLookup` to resolve the name `read_file`. The host must return the function object. Then when the function is called, Monty yields `RunProgress::FunctionCall`. This is a two-step process.

3. **OS operations use `OsFunction` enum** -- File operations like `Path("/foo").read_text()` and `os.getenv("X")` yield `RunProgress::OsCall` with typed `OsFunction` variants, NOT `RunProgress::FunctionCall`. The host handles these directly.

4. **Resource limits use `Option<usize>` / `Option<Duration>`** -- Not `u64` with 0-means-unlimited. Use `None` for unlimited, `Some(n)` for limited. The spec's `max_memory_bytes: u64` maps to `ResourceLimits { max_memory: Some(bytes as usize) }`.

5. **`MontyRun::start()` consumes `self`** -- Each execution creates a fresh `MontyRun` and consumes it via `start()`. The `MontyRun` is not reusable after `start()` (but it is reusable via `run()` which takes `&self`).

6. **`PrintWriter::Collect(&'a mut String)`** -- Output is captured by passing a mutable string reference, not by reading a buffer after execution.

7. **The spec says Python calls `read_file(path)`** -- In reality, Monty's standard library uses `Path(path).read_text()` which yields `OsCall(OsFunction::ReadText)`. For the spec's API (bare `read_file` function), we need to handle `NameLookup` + `FunctionCall` for user-defined external functions, OR adapt to use Monty's native `Path` API and handle `OsCall` instead. **Recommendation: handle BOTH patterns** -- `OsCall` for Monty's native Path/os operations AND `FunctionCall`/`NameLookup` for explicitly registered external functions.

### Design decision: external function strategy

**Option A (recommended):** Lean into Monty's native `OsCall` model. Monty already intercepts `Path.read_text()`, `Path.write_text()`, `os.getenv()`, etc. and yields `OsCall`. The host handles these by dispatching through `AgentCell`. This gives Python code a natural API (`from pathlib import Path; Path("/foo").read_text()`) instead of a custom one (`read_file("/foo")`). For HTTP (which Monty does not natively support), register external functions via `NameLookup` + `FunctionCall`.

**Option B:** Register all six functions as external functions via `NameLookup`. This means handling `NameLookup` for every function name, returning a callable, then handling `FunctionCall` when it is invoked. More work, but matches the spec exactly.

**Recommendation:** Implement Option A for fs/env operations (using `OsCall`) and Option B for `http_get`/`http_post` (which have no Monty-native equivalent). Document this deviation from the spec.

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/simulacra-python/Cargo.toml` | Create | Crate manifest with monty git dep |
| `crates/simulacra-python/src/lib.rs` | Create | Public API: `PythonRuntime`, `PyExecTool`, re-exports |
| `crates/simulacra-python/src/runtime.rs` | Create | `PythonRuntime` -- MontyRun creation, resource limit mapping |
| `crates/simulacra-python/src/dispatch.rs` | Create | External function dispatch loop (OsCall + FunctionCall handling) |
| `crates/simulacra-python/src/convert.rs` | Create | MontyObject <-> serde_json::Value conversion |
| `crates/simulacra-python/src/error.rs` | Create | `PythonError` enum |
| `crates/simulacra-python/tests/python_engine_tests.rs` | Create | Behavioral tests |
| `crates/simulacra-types/src/capability.rs` | Modify | Add `check_python()` method |
| `crates/simulacra-types/src/budget.rs` | Modify | Add Python resource limit fields |
| `crates/simulacra-sandbox/src/lib.rs` | Modify | Add `execute_py()` method to `AgentCell` |
| `crates/simulacra-tool/src/lib.rs` | Modify | Add `PyExecTool`, register in `register_builtins` behind feature |
| `crates/simulacra-tool/Cargo.toml` | Modify | Add optional `simulacra-python` dep behind `python` feature |
| `crates/simulacra-cli/Cargo.toml` | Modify | Add `python` feature forwarding to simulacra-tool |
| `crates/simulacra-cli/src/lib.rs` | Modify | Feature-gated bootstrap wiring |
| `Cargo.toml` | Modify | Add `simulacra-python` to workspace members + deps |

---

### Task 1: Scaffold `simulacra-python` crate with Monty dependency

**Files:**
- Create: `crates/simulacra-python/Cargo.toml`
- Create: `crates/simulacra-python/src/lib.rs`
- Create: `crates/simulacra-python/src/error.rs`
- Modify: `Cargo.toml` (workspace)

This task creates the crate skeleton, error types, and verifies monty compiles from git.

- [ ] **Step 1: Create the crate directory structure**

```bash
mkdir -p crates/simulacra-python/src
```

- [ ] **Step 2: Create `Cargo.toml`**

Create `crates/simulacra-python/Cargo.toml`:

```toml
[package]
name = "simulacra-python"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
simulacra-types.workspace = true
monty = { git = "https://github.com/pydantic/monty" }
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
opentelemetry.workspace = true
thiserror.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["full"] }
```

- [ ] **Step 3: Create error types**

Create `crates/simulacra-python/src/error.rs`:

```rust
use monty::MontyException;

/// Errors from Python execution.
#[derive(Debug, thiserror::Error)]
pub enum PythonError {
    #[error("parse error: {0}")]
    ParseError(String),

    #[error("execution error: {0}")]
    ExecutionError(String),

    #[error("resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),

    #[error("external function error: {0}")]
    ExternalFunctionError(String),
}

impl From<MontyException> for PythonError {
    fn from(exc: MontyException) -> Self {
        let msg = format_exception(&exc);
        // Check if this is a resource error (uncatchable exceptions from Monty)
        match exc.exc_type() {
            monty::ExcType::MemoryError => Self::ResourceLimitExceeded(msg),
            monty::ExcType::TimeoutError => Self::ResourceLimitExceeded(msg),
            monty::ExcType::RecursionError => Self::ResourceLimitExceeded(msg),
            _ => Self::ExecutionError(msg),
        }
    }
}

/// Format a MontyException into a human-readable string with traceback.
pub fn format_exception(exc: &MontyException) -> String {
    let mut parts = Vec::new();
    for frame in exc.traceback() {
        parts.push(format!(
            "  File \"{}\", line {}",
            frame.file_name(),
            frame.line()
        ));
    }
    let exc_type = exc.exc_type();
    match exc.message() {
        Some(msg) => parts.push(format!("{exc_type}: {msg}")),
        None => parts.push(format!("{exc_type}")),
    }
    parts.join("\n")
}
```

**Note for implementer:** Verify `StackFrame` field accessors. The actual method names may be `file_name()` and `line()` or similar -- check `exception_public.rs` in the monty crate. Adapt as needed.

- [ ] **Step 4: Create `lib.rs` with module structure**

Create `crates/simulacra-python/src/lib.rs`:

```rust
//! Monty Python engine for Simulacra.
//!
//! Wraps the Monty interpreter (pydantic/monty) to provide sandboxed Python
//! execution with external function mediation through the Golden Rule.

mod error;

pub use error::{PythonError, format_exception};
```

- [ ] **Step 5: Add to workspace**

In the root `Cargo.toml`, add `"crates/simulacra-python"` to the `members` array and add the workspace dependency:

```toml
# In [workspace] members:
"crates/simulacra-python",

# In [workspace.dependencies]:
simulacra-python = { path = "crates/simulacra-python" }
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo build -p simulacra-python`
Expected: PASS -- monty downloads from git and compiles. This will be slow on first build (monty pulls in ruff parser, salsa, etc.).

If it fails:
- Check Rust version >= 1.90.0 (monty requires edition 2024)
- Check that the monty git dep resolves correctly
- Check for dependency conflicts with ruff crates (monty uses ruff for Python parsing)

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-python/ Cargo.toml Cargo.lock
git commit -m "feat(python): scaffold simulacra-python crate with Monty dependency [S028]"
```

---

### Task 2: PythonRuntime -- create MontyRun, configure resource limits, execute code

**Files:**
- Create: `crates/simulacra-python/src/runtime.rs`
- Create: `crates/simulacra-python/src/convert.rs`
- Modify: `crates/simulacra-python/src/lib.rs`

This task implements the core execution engine: parsing Python code, configuring resource limits, running to completion (no external functions yet), and capturing print output.

- [ ] **Step 1: Create MontyObject conversion utilities**

Create `crates/simulacra-python/src/convert.rs`:

```rust
use monty::MontyObject;
use serde_json::Value;

/// Convert a MontyObject to a serde_json::Value.
pub fn monty_to_json(obj: &MontyObject) -> Value {
    match obj {
        MontyObject::None => Value::Null,
        MontyObject::Bool(b) => Value::Bool(*b),
        MontyObject::Int(n) => Value::Number(serde_json::Number::from(*n)),
        MontyObject::Float(f) => {
            serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null)
        }
        MontyObject::String(s) => Value::String(s.clone()),
        MontyObject::List(items) => {
            Value::Array(items.iter().map(monty_to_json).collect())
        }
        MontyObject::Tuple(items) => {
            Value::Array(items.iter().map(monty_to_json).collect())
        }
        MontyObject::Dict(pairs) => {
            let map: serde_json::Map<String, Value> = pairs
                .iter()
                .filter_map(|(k, v)| {
                    if let MontyObject::String(key) = k {
                        Some((key.clone(), monty_to_json(v)))
                    } else {
                        None // JSON only supports string keys
                    }
                })
                .collect();
            Value::Object(map)
        }
        _ => Value::String(format!("{obj:?}")),
    }
}

/// Convert a serde_json::Value to a MontyObject.
pub fn json_to_monty(val: &Value) -> MontyObject {
    match val {
        Value::Null => MontyObject::None,
        Value::Bool(b) => MontyObject::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else if let Some(f) = n.as_f64() {
                MontyObject::Float(f)
            } else {
                MontyObject::None
            }
        }
        Value::String(s) => MontyObject::String(s.clone()),
        Value::Array(arr) => {
            MontyObject::List(arr.iter().map(json_to_monty).collect())
        }
        Value::Object(map) => {
            let pairs: Vec<(MontyObject, MontyObject)> = map
                .iter()
                .map(|(k, v)| (MontyObject::String(k.clone()), json_to_monty(v)))
                .collect();
            MontyObject::Dict(monty::DictPairs::from(pairs))
        }
    }
}
```

**Note for implementer:** Check how `DictPairs` is constructed. It may be `DictPairs::new(vec)` or `From<Vec<(MontyObject, MontyObject)>>`. Inspect `monty/src/object.rs` for the actual API.

- [ ] **Step 2: Create PythonRuntime**

Create `crates/simulacra-python/src/runtime.rs`:

```rust
use std::time::Duration;

use monty::{
    LimitedTracker, MontyRun, NoLimitTracker, PrintWriter, ResourceLimits,
    RunProgress, MontyObject, ResourceTracker,
};

use crate::error::PythonError;

/// Resource limits for Python execution, mapped from Simulacra's ResourceBudget.
#[derive(Debug, Clone, Default)]
pub struct PythonResourceLimits {
    /// Maximum heap memory in bytes. None = unlimited.
    pub max_memory: Option<usize>,
    /// Maximum number of heap allocations. None = unlimited.
    pub max_allocations: Option<usize>,
    /// Maximum recursion depth. None = Monty default (1000).
    pub max_recursion_depth: Option<usize>,
    /// Maximum execution time. None = unlimited.
    pub max_duration: Option<Duration>,
}

/// Output from a Python execution.
#[derive(Debug, Clone, Default)]
pub struct PythonOutput {
    /// All text written via `print()`, including trailing newlines.
    pub stdout: String,
    /// The final result of the expression, if any.
    pub result: Option<MontyObject>,
}

/// Core Python execution engine wrapping Monty.
///
/// Each `execute()` call creates a fresh `MontyRun` -- no state persists
/// between calls.
pub struct PythonRuntime {
    limits: PythonResourceLimits,
}

impl PythonRuntime {
    /// Create a new PythonRuntime with the given resource limits.
    pub fn new(limits: PythonResourceLimits) -> Self {
        Self { limits }
    }

    /// Build Monty's ResourceLimits from our config.
    fn build_resource_limits(&self) -> ResourceLimits {
        let mut rl = ResourceLimits::new(); // default: recursion = 1000
        if let Some(mem) = self.limits.max_memory {
            rl = rl.max_memory(mem);
        }
        if let Some(alloc) = self.limits.max_allocations {
            rl = rl.max_allocations(alloc);
        }
        if let Some(depth) = self.limits.max_recursion_depth {
            rl = rl.max_recursion_depth(Some(depth));
        }
        if let Some(dur) = self.limits.max_duration {
            rl = rl.max_duration(dur);
        }
        rl
    }

    /// Execute Python code with no external function support.
    ///
    /// This is the simple path: run to completion, capture print output,
    /// return the result. Any external function call will raise NameError.
    pub fn execute_simple(&self, code: &str) -> Result<PythonOutput, PythonError> {
        let runner = MontyRun::new(code.to_owned(), "<py_exec>", vec![])
            .map_err(|e| PythonError::ParseError(format!("{e}")))?;

        let mut stdout = String::new();
        let limits = self.build_resource_limits();
        let tracker = LimitedTracker::new(limits);
        let print = PrintWriter::Collect(&mut stdout);

        let result = runner.run(vec![], tracker, print)
            .map_err(PythonError::from)?;

        Ok(PythonOutput {
            stdout,
            result: Some(result),
        })
    }

    /// Returns the configured resource limits (for building a LimitedTracker).
    pub fn resource_limits(&self) -> &PythonResourceLimits {
        &self.limits
    }

    pub fn build_tracker(&self) -> LimitedTracker {
        LimitedTracker::new(self.build_resource_limits())
    }
}
```

- [ ] **Step 3: Update lib.rs**

```rust
//! Monty Python engine for Simulacra.

mod convert;
mod error;
mod runtime;

pub use convert::{json_to_monty, monty_to_json};
pub use error::{PythonError, format_exception};
pub use runtime::{PythonOutput, PythonResourceLimits, PythonRuntime};
```

- [ ] **Step 4: Write basic execution tests**

Add to `crates/simulacra-python/tests/python_engine_tests.rs`:

```rust
use simulacra_python::{PythonRuntime, PythonResourceLimits};

fn make_runtime() -> PythonRuntime {
    PythonRuntime::new(PythonResourceLimits::default())
}

#[test]
fn print_hello() {
    let rt = make_runtime();
    let out = rt.execute_simple("print('hello')").unwrap();
    assert_eq!(out.stdout, "hello\n");
}

#[test]
fn print_arithmetic() {
    let rt = make_runtime();
    let out = rt.execute_simple("print(2 + 2)").unwrap();
    assert_eq!(out.stdout, "4\n");
}

#[test]
fn print_variable() {
    let rt = make_runtime();
    let out = rt.execute_simple("x = 42\nprint(x)").unwrap();
    assert_eq!(out.stdout, "42\n");
}

#[test]
fn multiple_prints() {
    let rt = make_runtime();
    let out = rt.execute_simple("print('a')\nprint('b')").unwrap();
    assert_eq!(out.stdout, "a\nb\n");
}

#[test]
fn syntax_error_returns_parse_error() {
    let rt = make_runtime();
    let err = rt.execute_simple("def broken(").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("parse error") || msg.contains("SyntaxError"), "got: {msg}");
}

#[test]
fn uncaught_exception_returns_execution_error() {
    let rt = make_runtime();
    let err = rt.execute_simple("raise ValueError('boom')").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("ValueError") || msg.contains("boom"), "got: {msg}");
}

#[test]
fn no_state_persists_between_calls() {
    let rt = make_runtime();
    rt.execute_simple("x = 42").unwrap();
    let err = rt.execute_simple("print(x)").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("NameError") || msg.contains("not defined"), "got: {msg}");
}
```

- [ ] **Step 5: Verify tests pass**

Run: `cargo test -p simulacra-python`
Expected: All basic execution tests pass.

**Note for implementer:** If `MontyRun::run()` does not capture print output correctly (e.g., if `PrintWriter::Collect` only captures from `print()` and not expression results), adjust. The spec says `print()` output is the tool result. If code produces no `print()` calls, result should be empty string.

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-python/
git commit -m "feat(python): PythonRuntime with MontyRun execution and resource limits [S028]"
```

---

### Task 3: External function dispatch -- pause/resume, map to Simulacra operations

**Files:**
- Create: `crates/simulacra-python/src/dispatch.rs`
- Modify: `crates/simulacra-python/src/runtime.rs`
- Modify: `crates/simulacra-python/src/lib.rs`

This task implements the pause/resume execution loop that handles `RunProgress::OsCall`, `RunProgress::FunctionCall`, and `RunProgress::NameLookup` by dispatching through a callback trait.

- [ ] **Step 1: Define the dispatch callback trait**

Create `crates/simulacra-python/src/dispatch.rs`:

```rust
use monty::{
    ExtFunctionResult, ExcType, FunctionCall, LimitedTracker, MontyException,
    MontyObject, MontyRun, NameLookup, NameLookupResult, OsCall, OsFunction,
    PrintWriter, RunProgress,
};

use crate::error::PythonError;
use crate::runtime::PythonOutput;

/// Trait for handling external operations during Python execution.
///
/// Implementations dispatch to AgentCell or test fakes.
/// All methods are synchronous because Monty's pause/resume is synchronous.
pub trait ExternalDispatcher: Send + Sync {
    /// Read file contents as text.
    fn read_file(&self, path: &str) -> Result<String, String>;
    /// Write text to a file.
    fn write_file(&self, path: &str, content: &str) -> Result<(), String>;
    /// List directory entries.
    fn list_dir(&self, path: &str) -> Result<Vec<String>, String>;
    /// HTTP GET request.
    fn http_get(&self, url: &str) -> Result<String, String>;
    /// HTTP POST request.
    fn http_post(&self, url: &str, body: &str) -> Result<String, String>;
    /// Read environment variable.
    fn env_get(&self, name: &str) -> Result<Option<String>, String>;
}

/// Known external function names that we register via NameLookup.
const EXTERNAL_FUNCTIONS: &[&str] = &[
    "read_file", "write_file", "list_dir",
    "http_get", "http_post", "env",
];

/// Execute Python code with external function dispatch.
///
/// Uses MontyRun::start() for pause/resume execution. Handles:
/// - RunProgress::OsCall -- Monty's native OS operations (Path.read_text, os.getenv, etc.)
/// - RunProgress::FunctionCall -- user-registered external functions (http_get, http_post)
/// - RunProgress::NameLookup -- resolve external function names
/// - RunProgress::Complete -- execution finished
pub fn execute_with_dispatch(
    code: &str,
    tracker: LimitedTracker,
    dispatcher: &dyn ExternalDispatcher,
) -> Result<PythonOutput, PythonError> {
    let runner = MontyRun::new(code.to_owned(), "<py_exec>", vec![])
        .map_err(|e| PythonError::ParseError(format!("{e}")))?;

    let mut stdout = String::new();
    let mut print = PrintWriter::Collect(&mut stdout);

    let mut progress = runner
        .start(vec![], tracker, print.reborrow())
        .map_err(PythonError::from)?;

    loop {
        match progress {
            RunProgress::Complete(result) => {
                return Ok(PythonOutput {
                    stdout,
                    result: Some(result),
                });
            }

            RunProgress::OsCall(call) => {
                let result = handle_os_call(&call, dispatcher);
                progress = call.resume(result, print.reborrow())
                    .map_err(PythonError::from)?;
            }

            RunProgress::FunctionCall(call) => {
                let result = handle_function_call(&call, dispatcher);
                progress = call.resume(result, print.reborrow())
                    .map_err(PythonError::from)?;
            }

            RunProgress::NameLookup(lookup) => {
                let result = handle_name_lookup(&lookup);
                progress = lookup.resume(result, print.reborrow())
                    .map_err(PythonError::from)?;
            }

            RunProgress::ResolveFutures(futures) => {
                // We don't use async external functions, so this shouldn't happen.
                // If it does, return an error.
                return Err(PythonError::ExecutionError(
                    "unexpected async futures in synchronous execution".into(),
                ));
            }
        }
    }
}

/// Handle Monty's native OS operations.
fn handle_os_call(
    call: &OsCall<LimitedTracker>,
    dispatcher: &dyn ExternalDispatcher,
) -> ExtFunctionResult {
    match call.function {
        OsFunction::ReadText => {
            // First arg is the Path object (as a string)
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.read_file(&p) {
                    Ok(content) => ExtFunctionResult::Return(MontyObject::String(content)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("read_text requires a path argument".into()),
                )),
            }
        }
        OsFunction::WriteText => {
            let path = extract_string_arg(&call.args, 0);
            let content = extract_string_arg(&call.args, 1);
            match (path, content) {
                (Some(p), Some(c)) => match dispatcher.write_file(&p, &c) {
                    Ok(()) => ExtFunctionResult::Return(MontyObject::None),
                    Err(e) => permission_or_runtime_error(&e),
                },
                _ => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("write_text requires path and content arguments".into()),
                )),
            }
        }
        OsFunction::Iterdir => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.list_dir(&p) {
                    Ok(entries) => {
                        let list = entries
                            .into_iter()
                            .map(MontyObject::String)
                            .collect();
                        ExtFunctionResult::Return(MontyObject::List(list))
                    }
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("iterdir requires a path argument".into()),
                )),
            }
        }
        OsFunction::Getenv => {
            let name = extract_string_arg(&call.args, 0);
            match name {
                Some(n) => match dispatcher.env_get(&n) {
                    Ok(Some(val)) => ExtFunctionResult::Return(MontyObject::String(val)),
                    Ok(None) => ExtFunctionResult::Return(MontyObject::None),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("getenv requires a name argument".into()),
                )),
            }
        }
        OsFunction::Exists | OsFunction::IsFile | OsFunction::IsDir => {
            // These are read-like operations -- delegate to read_file and check existence
            // For now, return False for unsupported OS calls
            ExtFunctionResult::Return(MontyObject::Bool(false))
        }
        _ => {
            // Unsupported OS operation
            ExtFunctionResult::Error(MontyException::new(
                ExcType::PermissionError,
                Some(format!("OS operation not supported in sandbox: {:?}", call.function)),
            ))
        }
    }
}

/// Handle user-registered external function calls.
fn handle_function_call(
    call: &FunctionCall<LimitedTracker>,
    dispatcher: &dyn ExternalDispatcher,
) -> ExtFunctionResult {
    match call.function_name.as_str() {
        "read_file" => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.read_file(&p) {
                    Ok(content) => ExtFunctionResult::Return(MontyObject::String(content)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("read_file(path) requires a string argument".into()),
                )),
            }
        }
        "write_file" => {
            let path = extract_string_arg(&call.args, 0);
            let content = extract_string_arg(&call.args, 1);
            match (path, content) {
                (Some(p), Some(c)) => match dispatcher.write_file(&p, &c) {
                    Ok(()) => ExtFunctionResult::Return(MontyObject::None),
                    Err(e) => permission_or_runtime_error(&e),
                },
                _ => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("write_file(path, content) requires two string arguments".into()),
                )),
            }
        }
        "list_dir" => {
            let path = extract_string_arg(&call.args, 0);
            match path {
                Some(p) => match dispatcher.list_dir(&p) {
                    Ok(entries) => {
                        let list = entries.into_iter().map(MontyObject::String).collect();
                        ExtFunctionResult::Return(MontyObject::List(list))
                    }
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("list_dir(path) requires a string argument".into()),
                )),
            }
        }
        "http_get" => {
            let url = extract_string_arg(&call.args, 0);
            match url {
                Some(u) => match dispatcher.http_get(&u) {
                    Ok(body) => ExtFunctionResult::Return(MontyObject::String(body)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("http_get(url) requires a string argument".into()),
                )),
            }
        }
        "http_post" => {
            let url = extract_string_arg(&call.args, 0);
            let body = extract_string_arg(&call.args, 1);
            match (url, body) {
                (Some(u), Some(b)) => match dispatcher.http_post(&u, &b) {
                    Ok(resp) => ExtFunctionResult::Return(MontyObject::String(resp)),
                    Err(e) => permission_or_runtime_error(&e),
                },
                _ => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("http_post(url, body) requires two string arguments".into()),
                )),
            }
        }
        "env" => {
            let name = extract_string_arg(&call.args, 0);
            match name {
                Some(n) => match dispatcher.env_get(&n) {
                    Ok(Some(val)) => ExtFunctionResult::Return(MontyObject::String(val)),
                    Ok(None) => ExtFunctionResult::Return(MontyObject::None),
                    Err(e) => permission_or_runtime_error(&e),
                },
                None => ExtFunctionResult::Error(MontyException::new(
                    ExcType::TypeError,
                    Some("env(name) requires a string argument".into()),
                )),
            }
        }
        _ => ExtFunctionResult::NotFound(call.function_name.clone()),
    }
}

/// Handle name lookups for external functions.
///
/// When Python code references `read_file`, `http_get`, etc., Monty yields a NameLookup.
/// We return a sentinel MontyObject that Monty will later call as a function,
/// yielding a FunctionCall.
fn handle_name_lookup(lookup: &NameLookup<LimitedTracker>) -> NameLookupResult {
    if EXTERNAL_FUNCTIONS.contains(&lookup.name.as_str()) {
        // Return a callable sentinel -- Monty needs an ExternalFunction object.
        // This is the tricky part: we need to understand what Monty expects
        // as a "callable" MontyObject for NameLookup resolution.
        //
        // **IMPLEMENTER NOTE:** This requires investigation. Monty may need a
        // specific MontyObject variant to represent an external callable.
        // Check if there's a MontyObject::ExternalFunction or similar.
        // If not, the NameLookup + FunctionCall pattern may not work for
        // bare function names, and we should rely on OsCall exclusively
        // (using Path().read_text() instead of read_file()).
        //
        // Fallback: return Undefined and let FunctionCall handle it instead.
        NameLookupResult::Undefined
    } else {
        NameLookupResult::Undefined
    }
}

/// Extract a string argument from MontyObject args at the given index.
fn extract_string_arg(args: &[MontyObject], index: usize) -> Option<String> {
    args.get(index).and_then(|obj| {
        if let MontyObject::String(s) = obj {
            Some(s.clone())
        } else {
            None
        }
    })
}

/// Convert an error message to the appropriate Python exception.
///
/// If the error message contains "capability denied" or "permission",
/// return PermissionError. If it contains "budget", return RuntimeError.
/// Otherwise, return RuntimeError.
fn permission_or_runtime_error(msg: &str) -> ExtFunctionResult {
    let lower = msg.to_lowercase();
    let exc_type = if lower.contains("capability denied") || lower.contains("permission") {
        ExcType::PermissionError
    } else {
        ExcType::RuntimeError
    };
    ExtFunctionResult::Error(MontyException::new(exc_type, Some(msg.to_string())))
}
```

**CRITICAL IMPLEMENTER NOTES:**

1. **NameLookup for external functions:** The `NameLookup` -> `FunctionCall` pattern requires returning a callable `MontyObject` from `handle_name_lookup`. Investigate what Monty expects. If there is no `MontyObject::ExternalFunction` variant, the bare function name pattern (`read_file("path")`) won't work, and you should use Monty's native Path API instead (`Path("/foo").read_text()`) which yields `OsCall`.

2. **OsCall arg extraction:** When Monty yields `OsCall(OsFunction::ReadText)`, the first arg may be the `Path` object itself (as a MontyObject), not a string. Check how Monty serializes Path objects in `OsCall.args`. You may need to extract the path string from a structured object.

3. **HTTP has no OsCall equivalent:** `http_get` and `http_post` must go through the `FunctionCall` path. If `NameLookup` cannot return callables, you may need to teach the Python code to use a different pattern (e.g., provide a helper module).

- [ ] **Step 2: Add execute_with_dispatch to PythonRuntime**

Add to `crates/simulacra-python/src/runtime.rs`:

```rust
use crate::dispatch::{ExternalDispatcher, execute_with_dispatch};

impl PythonRuntime {
    /// Execute Python code with external function dispatch.
    ///
    /// External function calls (OsCall, FunctionCall) are routed to the dispatcher.
    pub fn execute(
        &self,
        code: &str,
        dispatcher: &dyn ExternalDispatcher,
    ) -> Result<PythonOutput, PythonError> {
        let tracker = self.build_tracker();
        execute_with_dispatch(code, tracker, dispatcher)
    }
}
```

- [ ] **Step 3: Update lib.rs**

```rust
mod convert;
mod dispatch;
mod error;
mod runtime;

pub use convert::{json_to_monty, monty_to_json};
pub use dispatch::ExternalDispatcher;
pub use error::{PythonError, format_exception};
pub use runtime::{PythonOutput, PythonResourceLimits, PythonRuntime};
```

- [ ] **Step 4: Write dispatch tests with a fake dispatcher**

Add to `crates/simulacra-python/tests/python_engine_tests.rs`:

```rust
use std::collections::HashMap;
use simulacra_python::ExternalDispatcher;

struct FakeDispatcher {
    files: HashMap<String, String>,
    env: HashMap<String, String>,
}

impl FakeDispatcher {
    fn new() -> Self {
        let mut files = HashMap::new();
        files.insert("/data.txt".into(), "hello world".into());
        let mut env = HashMap::new();
        env.insert("MY_VAR".into(), "my_value".into());
        Self { files, env }
    }
}

impl ExternalDispatcher for FakeDispatcher {
    fn read_file(&self, path: &str) -> Result<String, String> {
        self.files.get(path).cloned().ok_or_else(|| format!("file not found: {path}"))
    }
    fn write_file(&self, _path: &str, _content: &str) -> Result<(), String> {
        Ok(())
    }
    fn list_dir(&self, _path: &str) -> Result<Vec<String>, String> {
        Ok(vec!["file1.txt".into(), "file2.txt".into()])
    }
    fn http_get(&self, url: &str) -> Result<String, String> {
        Ok(format!("response from {url}"))
    }
    fn http_post(&self, url: &str, body: &str) -> Result<String, String> {
        Ok(format!("posted to {url}: {body}"))
    }
    fn env_get(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.env.get(name).cloned())
    }
}

// Tests for OsCall-based external functions
// These tests use Monty's native Path API

#[test]
fn os_call_read_text() {
    let rt = make_runtime();
    let dispatcher = FakeDispatcher::new();
    // Uses Monty's native Path.read_text() which yields OsCall::ReadText
    let out = rt.execute(
        "from pathlib import Path\nprint(Path('/data.txt').read_text())",
        &dispatcher,
    ).unwrap();
    assert_eq!(out.stdout, "hello world\n");
}

#[test]
fn os_call_getenv() {
    let rt = make_runtime();
    let dispatcher = FakeDispatcher::new();
    let out = rt.execute(
        "import os\nprint(os.getenv('MY_VAR'))",
        &dispatcher,
    ).unwrap();
    assert_eq!(out.stdout, "my_value\n");
}

#[test]
fn os_call_getenv_missing_returns_none() {
    let rt = make_runtime();
    let dispatcher = FakeDispatcher::new();
    let out = rt.execute(
        "import os\nprint(os.getenv('MISSING'))",
        &dispatcher,
    ).unwrap();
    assert_eq!(out.stdout, "None\n");
}
```

**Note:** These tests depend on Monty's stdlib support for `pathlib` and `os`. If Monty does not support `from pathlib import Path`, the OsCall path may not be triggered. In that case, investigate how Monty triggers OsCall and adapt the test code accordingly.

- [ ] **Step 5: Verify tests pass**

Run: `cargo test -p simulacra-python`

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-python/
git commit -m "feat(python): external function dispatch with OsCall/FunctionCall handling [S028]"
```

---

### Task 4: PyExecTool -- Tool trait implementation

**Files:**
- Modify: `crates/simulacra-types/src/capability.rs` -- add `check_python()`
- Modify: `crates/simulacra-sandbox/src/lib.rs` -- add `execute_py()` to AgentCell
- Modify: `crates/simulacra-tool/Cargo.toml` -- add optional simulacra-python dep
- Modify: `crates/simulacra-tool/src/lib.rs` -- add `PyExecTool` + register

This task wires `PythonRuntime` into the existing tool system, following the exact pattern of `JsExecTool`.

- [ ] **Step 1: Add `check_python()` to CapabilityToken**

In `crates/simulacra-types/src/capability.rs`, add after `check_javascript()`:

```rust
/// Check if Python execution is allowed.
pub fn check_python(&self) -> Result<(), CapabilityDenied> {
    if self.python {
        Ok(())
    } else {
        Err(CapabilityDenied {
            operation: "python".into(),
            reason: "python execution is not allowed by this capability token".into(),
        })
    }
}
```

- [ ] **Step 2: Add `execute_py()` to AgentCell**

In `crates/simulacra-sandbox/src/lib.rs`, add an `execute_py()` method to `AgentCell` following the same pattern as `execute_js()`:

```rust
// Behind #[cfg(feature = "python")]
/// Execute Python code, checking python capability and turns budget.
pub fn execute_py(&self, code: &str) -> Result<simulacra_python::PythonOutput, SandboxError> {
    let _span = tracing::info_span!(
        "sandbox_py_exec",
        simulacra.operation.name = "sandbox_py_exec",
    ).entered();

    // Check capability
    check_and_journal_capability(
        || self.capability.check_python(),
        "execute_py",
        "python",
        &self.journal,
        &self.agent_id,
    )?;

    // Check turns budget
    check_turns_budget(&self.budget, &self.journal, &self.agent_id)?;

    // Build dispatcher that routes through AgentCell methods
    let dispatcher = AgentCellPyDispatcher { cell: self };
    let runtime = simulacra_python::PythonRuntime::new(Default::default());

    match runtime.execute(code, &dispatcher) {
        Ok(output) => {
            // Journal entry
            if let Err(err) = self.journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::CodeExecution {
                    language: "python".to_string(),
                },
            }) {
                tracing::error!(error = %err, "journal append failed for execute_py");
            }
            Ok(output)
        }
        Err(py_err) => {
            // Journal even on error
            if let Err(err) = self.journal.append(JournalEntry {
                schema_version: JOURNAL_SCHEMA_VERSION,
                agent_id: self.agent_id.clone(),
                timestamp_ms: 0,
                entry: JournalEntryKind::CodeExecution {
                    language: "python".to_string(),
                },
            }) {
                tracing::error!(error = %err, "journal append failed for execute_py");
            }
            Err(SandboxError::Python(py_err))
        }
    }
}
```

**Note:** This requires adding a `Python(simulacra_python::PythonError)` variant to `SandboxError`, and implementing `AgentCellPyDispatcher` that delegates `ExternalDispatcher` methods to existing `AgentCell` methods (read_file, write_file, etc.).

The `AgentCellPyDispatcher` should follow the same pattern as `AgentCellFsProxy`:

```rust
#[cfg(feature = "python")]
struct AgentCellPyDispatcher<'a> {
    cell: &'a AgentCell,
}

#[cfg(feature = "python")]
impl simulacra_python::ExternalDispatcher for AgentCellPyDispatcher<'_> {
    fn read_file(&self, path: &str) -> Result<String, String> {
        self.cell.read_file(path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .map_err(|e| e.to_string())
    }
    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.cell.write_file(path, content.as_bytes())
            .map_err(|e| e.to_string())
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>, String> {
        self.cell.list_dir(path)
            .map_err(|e| e.to_string())
    }
    fn http_get(&self, url: &str) -> Result<String, String> {
        // This needs async -- may need to block on a runtime.
        // AgentCell::fetch_http is async. For synchronous dispatch,
        // use tokio::runtime::Handle::current().block_on() or
        // restructure to support async dispatch.
        Err("http_get not yet supported in python sandbox".into())
    }
    fn http_post(&self, url: &str, body: &str) -> Result<String, String> {
        Err("http_post not yet supported in python sandbox".into())
    }
    fn env_get(&self, name: &str) -> Result<Option<String>, String> {
        // AgentCell doesn't have an env method yet -- need to add one
        // or read from capability-filtered env vars
        Ok(std::env::var(name).ok())
    }
}
```

**CRITICAL NOTE:** HTTP dispatch is async in Simulacra but Monty's pause/resume is synchronous. The implementer must decide:
- Option A: Use `tokio::task::block_in_place` + `Handle::current().block_on()` to bridge async/sync
- Option B: Make `ExternalDispatcher` async and restructure the dispatch loop to use async
- Option C: Defer HTTP support to a follow-up (implement fs + env first)

Recommend Option A for simplicity if the code runs within a tokio runtime.

- [ ] **Step 3: Add PyExecTool to simulacra-tool**

In `crates/simulacra-tool/Cargo.toml`, add:

```toml
[features]
python = ["dep:simulacra-python"]

[dependencies]
simulacra-python = { workspace = true, optional = true }
```

In `crates/simulacra-tool/src/lib.rs`, add `PyExecTool` after `JsExecTool`:

```rust
// ---------------------------------------------------------------------------
// PyExecTool
// ---------------------------------------------------------------------------

#[cfg(feature = "python")]
struct PyExecTool {
    cell: Arc<AgentCell>,
}

#[cfg(feature = "python")]
impl Tool for PyExecTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "py_exec".into(),
            description: "Execute Python code in the Monty Python runtime and return the result."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python code to execute." }
                },
                "required": ["code"]
            }),
        }
    }

    fn call(
        &self,
        args: Value,
        _capability: &CapabilityToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let code = require_str(&args, "code")?;

            match self.cell.execute_py(&code) {
                Ok(output) => {
                    Ok(json!(output.stdout))
                }
                Err(err) => {
                    Err(ToolError::ExecutionFailed(format!("{err}")))
                }
            }
        })
    }
}
```

- [ ] **Step 4: Register PyExecTool in `register_builtins`**

In `register_builtins()`, add after the `JsExecTool` registration:

```rust
#[cfg(feature = "python")]
registry.register(Box::new(PyExecTool {
    cell: Arc::clone(&cell),
}));
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build -p simulacra-tool --features python`

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-types/ crates/simulacra-sandbox/ crates/simulacra-tool/ crates/simulacra-python/
git commit -m "feat(python): PyExecTool with AgentCell dispatch and Tool trait impl [S028]"
```

---

### Task 5: Resource limit enforcement tests

**Files:**
- Modify: `crates/simulacra-python/tests/python_engine_tests.rs`

This task adds tests for all four resource limit types, verifying that Monty's limits are correctly configured and produce the expected error messages.

- [ ] **Step 1: Add resource limit tests**

```rust
use std::time::Duration;
use simulacra_python::PythonResourceLimits;

#[test]
fn memory_limit_exceeded() {
    let rt = PythonRuntime::new(PythonResourceLimits {
        max_memory: Some(1024), // 1KB -- very tight
        ..Default::default()
    });
    let err = rt.execute_simple("x = 'a' * 1000000").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("memory") || msg.contains("MemoryError"),
        "expected memory limit error, got: {msg}"
    );
}

#[test]
fn allocation_limit_exceeded() {
    let rt = PythonRuntime::new(PythonResourceLimits {
        max_allocations: Some(10), // very few allocations
        ..Default::default()
    });
    let err = rt.execute_simple("x = [i for i in range(1000)]").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("allocation") || msg.contains("MemoryError"),
        "expected allocation limit error, got: {msg}"
    );
}

#[test]
fn recursion_depth_exceeded() {
    let rt = PythonRuntime::new(PythonResourceLimits {
        max_recursion_depth: Some(10),
        ..Default::default()
    });
    let err = rt.execute_simple("def f(n): return f(n+1)\nf(0)").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("recursion") || msg.contains("RecursionError"),
        "expected recursion depth error, got: {msg}"
    );
}

#[test]
fn execution_time_exceeded() {
    let rt = PythonRuntime::new(PythonResourceLimits {
        max_duration: Some(Duration::from_millis(100)),
        ..Default::default()
    });
    let err = rt.execute_simple("while True: pass").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("time") || msg.contains("TimeoutError"),
        "expected time limit error, got: {msg}"
    );
}

#[test]
fn resource_limits_none_means_unlimited() {
    // Default limits (all None) should allow reasonable code
    let rt = PythonRuntime::new(PythonResourceLimits::default());
    let out = rt.execute_simple("x = [i for i in range(10000)]\nprint(len(x))").unwrap();
    assert_eq!(out.stdout, "10000\n");
}

#[test]
fn resource_counters_reset_between_invocations() {
    let rt = PythonRuntime::new(PythonResourceLimits {
        max_allocations: Some(100),
        ..Default::default()
    });
    // First call uses some allocations
    rt.execute_simple("x = [i for i in range(50)]").unwrap();
    // Second call should also work (counters reset)
    rt.execute_simple("x = [i for i in range(50)]").unwrap();
}
```

- [ ] **Step 2: Verify tests pass**

Run: `cargo test -p simulacra-python`

- [ ] **Step 3: Commit**

```bash
git add crates/simulacra-python/
git commit -m "test(python): resource limit enforcement tests [S028]"
```

---

### Task 6: Sandbox isolation tests

**Files:**
- Modify: `crates/simulacra-python/tests/python_engine_tests.rs`

This task verifies that Python code cannot escape the sandbox.

- [ ] **Step 1: Add sandbox isolation tests**

```rust
#[test]
fn cannot_import_subprocess() {
    let rt = make_runtime();
    let err = rt.execute_simple("import subprocess").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("ModuleNotFoundError") || msg.contains("No module"),
        "expected ModuleNotFoundError, got: {msg}"
    );
}

#[test]
fn cannot_import_socket() {
    let rt = make_runtime();
    let err = rt.execute_simple("import socket").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("ModuleNotFoundError") || msg.contains("No module"),
        "expected ModuleNotFoundError, got: {msg}"
    );
}

#[test]
fn cannot_import_ctypes() {
    let rt = make_runtime();
    let err = rt.execute_simple("import ctypes").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("ModuleNotFoundError") || msg.contains("No module"),
        "expected ModuleNotFoundError, got: {msg}"
    );
}

#[test]
fn os_environ_does_not_expose_host_vars() {
    // os.environ should be empty or not available in Monty sandbox
    let rt = make_runtime();
    // This should either raise an error or return an empty dict
    let result = rt.execute_simple("import os\nprint(len(os.environ))");
    match result {
        Ok(out) => assert_eq!(out.stdout, "0\n", "os.environ should be empty"),
        Err(_) => {} // Also acceptable if os.environ is not supported
    }
}

// Stdlib tests -- verify supported modules work
#[test]
fn import_json_works() {
    let rt = make_runtime();
    let out = rt.execute_simple(
        "import json\nprint(json.dumps({'a': 1}))"
    ).unwrap();
    assert!(out.stdout.contains("\"a\""), "got: {}", out.stdout);
    assert!(out.stdout.contains("1"), "got: {}", out.stdout);
}

#[test]
fn import_re_works() {
    let rt = make_runtime();
    let out = rt.execute_simple(
        "import re\nm = re.match(r'\\d+', '42abc')\nprint(m.group())"
    ).unwrap();
    assert_eq!(out.stdout, "42\n");
}

#[test]
fn import_datetime_works() {
    let rt = make_runtime();
    // datetime module should be available
    let out = rt.execute_simple(
        "from datetime import timedelta\nprint(timedelta(days=1).total_seconds())"
    ).unwrap();
    assert_eq!(out.stdout, "86400.0\n");
}

#[test]
fn unsupported_module_raises_error() {
    let rt = make_runtime();
    let err = rt.execute_simple("import numpy").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("ModuleNotFoundError") || msg.contains("No module"),
        "expected ModuleNotFoundError, got: {msg}"
    );
}
```

**Note for implementer:** Some of these tests depend on what Monty's stdlib actually supports. If `import re` does not work in Monty, adjust the test. Check Monty's docs/tests for confirmed stdlib support. The key behavioral property is that dangerous modules (`subprocess`, `socket`, `ctypes`) are NOT available.

- [ ] **Step 2: Verify tests pass**

Run: `cargo test -p simulacra-python`

- [ ] **Step 3: Commit**

```bash
git add crates/simulacra-python/
git commit -m "test(python): sandbox isolation and stdlib tests [S028]"
```

---

### Task 7: Config + CLI bootstrap wiring (feature-gated behind "python")

**Files:**
- Modify: `crates/simulacra-cli/Cargo.toml` -- add `python` feature
- Modify: `crates/simulacra-cli/src/lib.rs` -- feature-gated bootstrap
- Modify: `crates/simulacra-sandbox/Cargo.toml` -- add optional simulacra-python dep

This task wires the feature flag through the dependency chain so `cargo install simulacra --features python` includes the Python engine.

- [ ] **Step 1: Add feature to simulacra-sandbox**

In `crates/simulacra-sandbox/Cargo.toml`:

```toml
[features]
python = ["dep:simulacra-python"]

[dependencies]
simulacra-python = { workspace = true, optional = true }
```

- [ ] **Step 2: Add feature to simulacra-tool**

In `crates/simulacra-tool/Cargo.toml`:

```toml
[features]
python = ["simulacra-sandbox/python"]
```

Or if simulacra-tool directly depends on simulacra-python:

```toml
[features]
python = ["dep:simulacra-python", "simulacra-sandbox/python"]
```

- [ ] **Step 3: Add feature to simulacra-cli**

In `crates/simulacra-cli/Cargo.toml`:

```toml
[features]
default = []
python = ["simulacra-tool/python"]
```

- [ ] **Step 4: Feature-gate in CLI bootstrap**

In `crates/simulacra-cli/src/lib.rs`, the `py_exec` tool is already registered by `simulacra-tool/register_builtins` behind `#[cfg(feature = "python")]`. Verify the feature flag propagates correctly.

If there's any additional CLI-level setup needed (e.g., logging, capability defaults), add it behind `#[cfg(feature = "python")]`.

- [ ] **Step 5: Verify default build excludes Python**

Run: `cargo build -p simulacra-cli`
Expected: PASS -- builds without monty dependency.

- [ ] **Step 6: Verify feature build includes Python**

Run: `cargo build -p simulacra-cli --features python`
Expected: PASS -- monty is compiled and py_exec is available.

- [ ] **Step 7: Run full workspace checks**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Also test with the python feature:

```bash
cargo build --workspace --features simulacra-cli/python
cargo test --workspace --features simulacra-cli/python
```

- [ ] **Step 8: Commit**

```bash
git add crates/simulacra-cli/ crates/simulacra-tool/ crates/simulacra-sandbox/ Cargo.toml Cargo.lock
git commit -m "feat(python): feature-gated CLI bootstrap wiring [S028]"
```

---

## Open Questions for Implementer

1. **NameLookup callable resolution:** How does Monty expect external functions to be returned from `NameLookup`? Is there a `MontyObject` variant for external callables? If not, the bare function name API (`read_file("path")`) from the spec may not be feasible, and the implementation should use Monty's native Path/os API exclusively (which uses `OsCall`). For HTTP, an alternative approach may be needed.

2. **HTTP async bridge:** Simulacra's HTTP client is async. Monty's dispatch loop is synchronous. The implementer must bridge this gap. `tokio::task::block_in_place` + `Handle::current().block_on()` is the simplest approach if running inside a tokio multi-thread runtime.

3. **OsCall argument format:** When Monty yields `OsCall(OsFunction::ReadText, args)`, what are the `args`? Is the first arg the path as a `MontyObject::String`, or is it a structured Path object? The implementer should add a debug print and test to discover the actual format.

4. **`os.environ` behavior:** Monty intercepts `os.getenv()` via `OsCall(OsFunction::Getenv)`. But what about `os.environ` dict access? Check if Monty yields `OsCall(OsFunction::GetEnviron)` for dict-style access and handle it by returning an empty dict (sandbox property: no host env vars exposed).

5. **`datetime.date.today()` and `datetime.datetime.now()`:** Monty yields `OsCall(OsFunction::DateToday)` and `OsCall(OsFunction::DateTimeNow)`. The implementer should handle these by returning the current date/time (these are safe operations that don't leak sensitive info).

6. **Monty's edition 2024 requirement:** Monty requires Rust edition 2024 (Rust 1.90+). Verify the workspace Rust version is compatible.

7. **DictPairs construction:** Check how to construct `MontyObject::Dict`. The `DictPairs` type may have a specific constructor or implement `From<Vec<(MontyObject, MontyObject)>>`.

8. **`MontyRun::start()` consumes self:** Each call to `execute_with_dispatch` creates a new `MontyRun` and consumes it. This is correct for the "no state persists" property but means `PythonRuntime` itself does NOT hold a `MontyRun` -- it creates a fresh one per call.
