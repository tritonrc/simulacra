# S026 Governance Hook Pipeline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rack-style governance hook pipeline with JS runtime — hooks wrap agent operations (tool_call, llm, spawn, http_request) and can observe, modify, or deny.

**Architecture:** New `simulacra-hooks` crate defines the pipeline framework (Verdict, HookModule trait, HookChain, HookPipeline). JS hooks are evaluated in a minimal rquickjs context (not the full simulacra-quickjs sandbox). The pipeline is created at CLI bootstrap and threaded through to ToolRegistry, AgentLoop, SpawnAgentTool, and UreqHttpClient as `Arc<HookPipeline>`.

**Tech Stack:** Rust, rquickjs (for JS hooks), serde_json, tracing, opentelemetry

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/simulacra-hooks/Cargo.toml` | Create | Crate manifest |
| `crates/simulacra-hooks/src/lib.rs` | Create | Public API: types, HookPipeline, re-exports |
| `crates/simulacra-hooks/src/verdict.rs` | Create | Verdict, Phase, Operation enums |
| `crates/simulacra-hooks/src/pipeline.rs` | Create | HookChain, HookPipeline — onion execution |
| `crates/simulacra-hooks/src/error.rs` | Create | HookError enum |
| `crates/simulacra-hooks/src/js.rs` | Create | JsHookModule — rquickjs evaluation, timeout |
| `crates/simulacra-hooks/tests/hook_pipeline_tests.rs` | Create | Pipeline behavioral tests |
| `crates/simulacra-hooks/tests/js_hook_tests.rs` | Create | JS hook runtime tests |
| `crates/simulacra-hooks/fixtures/` | Create | Sample JS hooks for testing |
| `crates/simulacra-config/src/lib.rs` | Modify | Add HooksConfig, HookEntry |
| `crates/simulacra-tool/src/lib.rs` | Modify | Thread HookPipeline through ToolRegistry |
| `crates/simulacra-runtime/src/agent_loop.rs` | Modify | Thread HookPipeline, wrap provider.chat() and tool dispatch |
| `crates/simulacra-http/src/lib.rs` | Modify | Thread HookPipeline through HttpClient trait |
| `crates/simulacra-http/src/client.rs` | Modify | Wrap execute() with http_request hooks |
| `crates/simulacra-cli/src/lib.rs` | Modify | Build HookPipeline at bootstrap, pass to all consumers |
| `Cargo.toml` | Modify | Add simulacra-hooks to workspace |

---

### Task 1: Scaffold `simulacra-hooks` crate with core types

**Files:**
- Create: `crates/simulacra-hooks/Cargo.toml`
- Create: `crates/simulacra-hooks/src/lib.rs`
- Create: `crates/simulacra-hooks/src/verdict.rs`
- Create: `crates/simulacra-hooks/src/error.rs`
- Modify: `Cargo.toml` (workspace)

- [ ] **Step 1: Create crate structure**

```bash
mkdir -p crates/simulacra-hooks/src crates/simulacra-hooks/fixtures
```

- [ ] **Step 2: Create `Cargo.toml`**

Create `crates/simulacra-hooks/Cargo.toml`:

```toml
[package]
name = "simulacra-hooks"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
simulacra-types.workspace = true
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
opentelemetry.workspace = true
thiserror.workspace = true
rquickjs.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["full"] }
```

- [ ] **Step 3: Create verdict types**

Create `crates/simulacra-hooks/src/verdict.rs`:

```rust
/// What a hook returns after inspecting an operation.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Proceed. Optionally replace the context JSON.
    Continue { modified_context: Option<String> },
    /// Block the operation (before-phase only). It does not execute.
    Deny { reason: String },
    /// Terminate the agent immediately.
    Kill { reason: String },
}

/// Which side of the operation the hook is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    Before,
    After,
}

/// The operation types hooks can wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    ToolCall,
    Llm,
    Spawn,
    HttpRequest,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Before => write!(f, "before"),
            Phase::After => write!(f, "after"),
        }
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Operation::ToolCall => write!(f, "tool_call"),
            Operation::Llm => write!(f, "llm"),
            Operation::Spawn => write!(f, "spawn"),
            Operation::HttpRequest => write!(f, "http_request"),
        }
    }
}
```

- [ ] **Step 4: Create error types**

Create `crates/simulacra-hooks/src/error.rs`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("hook {hook} denied: {reason}")]
    Denied { hook: String, reason: String },

    #[error("hook {hook} killed agent: {reason}")]
    Killed { hook: String, reason: String },

    #[error("hook {hook} timed out after {timeout_ms}ms")]
    Timeout { hook: String, timeout_ms: u64 },

    #[error("hook {hook} execution error: {0}")]
    ExecutionError { hook: String, source: String },
}
```

- [ ] **Step 5: Create `lib.rs`**

Create `crates/simulacra-hooks/src/lib.rs`:

```rust
//! Governance hook pipeline for Simulacra.
//!
//! Rack-style middleware that wraps agent operations. Hooks can observe,
//! modify, or deny operations at lifecycle and I/O points. Runtime-agnostic
//! framework — JS hooks (S026), WASM hooks (future), Rust builtins (future).

mod error;
mod verdict;

pub use error::HookError;
pub use verdict::{Operation, Phase, Verdict};

/// Runtime-agnostic interface for hook execution.
///
/// Implemented by JsHookModule (this crate), WasmHookModule (future),
/// or Rust builtins (implement directly).
pub trait HookModule: Send + Sync {
    /// The hook's display name (for logging and errors).
    fn name(&self) -> &str;

    /// Invoke the hook for a given phase, operation, and JSON context.
    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError>;
}
```

- [ ] **Step 6: Add to workspace**

In root `Cargo.toml`:
- Add `"crates/simulacra-hooks"` to `members`
- Add `simulacra-hooks = { path = "crates/simulacra-hooks" }` to `[workspace.dependencies]`

- [ ] **Step 7: Verify compilation**

Run: `cargo build -p simulacra-hooks`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/simulacra-hooks/ Cargo.toml Cargo.lock
git commit -m "feat(hooks): scaffold simulacra-hooks crate with verdict model [S026]"
```

---

### Task 2: HookPipeline — onion execution engine

**Files:**
- Create: `crates/simulacra-hooks/src/pipeline.rs`
- Modify: `crates/simulacra-hooks/src/lib.rs`
- Create: `crates/simulacra-hooks/tests/hook_pipeline_tests.rs`

The pipeline drives the onion: before-hooks forward, execute, after-hooks reverse.

- [ ] **Step 1: Write failing tests**

Create `crates/simulacra-hooks/tests/hook_pipeline_tests.rs`:

```rust
use simulacra_hooks::{HookError, HookModule, HookPipeline, Operation, Phase, Verdict};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A test hook that records invocations and returns a configured verdict.
struct TestHook {
    hook_name: String,
    before_verdict: Verdict,
    after_verdict: Verdict,
    invocations: Arc<AtomicUsize>,
}

impl TestHook {
    fn new(name: &str, before: Verdict, after: Verdict) -> Self {
        Self {
            hook_name: name.to_string(),
            before_verdict: before,
            after_verdict: after,
            invocations: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn pass(name: &str) -> Self {
        Self::new(name, Verdict::continue_unchanged(), Verdict::continue_unchanged())
    }

    fn invocation_count(&self) -> usize {
        self.invocations.load(Ordering::SeqCst)
    }
}

impl HookModule for TestHook {
    fn name(&self) -> &str {
        &self.hook_name
    }

    fn invoke(
        &self,
        phase: Phase,
        _operation: Operation,
        _context: &str,
    ) -> Result<Verdict, HookError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        match phase {
            Phase::Before => Ok(self.before_verdict.clone()),
            Phase::After => Ok(self.after_verdict.clone()),
        }
    }
}

#[test]
fn empty_chain_passes_through() {
    let pipeline = HookPipeline::new();
    let result = pipeline.run_before(Operation::ToolCall, r#"{"tool":"echo"}"#);
    assert!(result.is_ok());
    let (verdict, _ctx) = result.unwrap();
    assert!(matches!(verdict, Verdict::Continue { .. }));
}

#[test]
fn before_hooks_run_in_config_order() {
    let hook1 = Arc::new(TestHook::pass("first"));
    let hook2 = Arc::new(TestHook::pass("second"));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook1.clone());
    pipeline.add(Operation::ToolCall, hook2.clone());

    let _ = pipeline.run_before(Operation::ToolCall, "{}");

    assert_eq!(hook1.invocation_count(), 1);
    assert_eq!(hook2.invocation_count(), 1);
}

#[test]
fn deny_in_before_stops_chain() {
    let deny_hook = Arc::new(TestHook::new(
        "denier",
        Verdict::Deny { reason: "blocked".into() },
        Verdict::continue_unchanged(),
    ));
    let after_hook = Arc::new(TestHook::pass("should-not-run"));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, deny_hook.clone());
    pipeline.add(Operation::ToolCall, after_hook.clone());

    let result = pipeline.run_before(Operation::ToolCall, "{}");
    assert!(matches!(result, Err(HookError::Denied { .. })));
    assert_eq!(after_hook.invocation_count(), 0, "hooks after deny should not run");
}

#[test]
fn kill_in_before_stops_chain() {
    let kill_hook = Arc::new(TestHook::new(
        "killer",
        Verdict::Kill { reason: "terminated".into() },
        Verdict::continue_unchanged(),
    ));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, kill_hook);

    let result = pipeline.run_before(Operation::ToolCall, "{}");
    assert!(matches!(result, Err(HookError::Killed { .. })));
}

#[test]
fn after_hooks_run_in_reverse_order() {
    // Use hooks that record their invocation order
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    struct OrderRecorder {
        name: String,
        order: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl HookModule for OrderRecorder {
        fn name(&self) -> &str { &self.name }
        fn invoke(&self, phase: Phase, _op: Operation, _ctx: &str) -> Result<Verdict, HookError> {
            if phase == Phase::After {
                self.order.lock().unwrap().push(self.name.clone());
            }
            Ok(Verdict::continue_unchanged())
        }
    }

    let hook1 = Arc::new(OrderRecorder { name: "first".into(), order: order.clone() });
    let hook2 = Arc::new(OrderRecorder { name: "second".into(), order: order.clone() });
    let hook3 = Arc::new(OrderRecorder { name: "third".into(), order: order.clone() });

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, hook1);
    pipeline.add(Operation::ToolCall, hook2);
    pipeline.add(Operation::ToolCall, hook3);

    let _ = pipeline.run_after(Operation::ToolCall, "{}");

    let recorded = order.lock().unwrap();
    assert_eq!(*recorded, vec!["third", "second", "first"], "after hooks should run in reverse");
}

#[test]
fn deny_in_after_is_logged_not_enforced() {
    let deny_hook = Arc::new(TestHook::new(
        "after-denier",
        Verdict::continue_unchanged(),
        Verdict::Deny { reason: "too late".into() },
    ));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, deny_hook);

    // After-phase deny should be treated as Continue (logged but not enforced)
    let result = pipeline.run_after(Operation::ToolCall, "{}");
    assert!(result.is_ok(), "deny in after-phase should not fail");
}

#[test]
fn kill_in_after_is_enforced() {
    let kill_hook = Arc::new(TestHook::new(
        "after-killer",
        Verdict::continue_unchanged(),
        Verdict::Kill { reason: "post-hoc kill".into() },
    ));

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, kill_hook);

    let result = pipeline.run_after(Operation::ToolCall, "{}");
    assert!(matches!(result, Err(HookError::Killed { .. })));
}

#[test]
fn modifications_chain_through_hooks() {
    struct ModifyHook { name: String, suffix: String }
    impl HookModule for ModifyHook {
        fn name(&self) -> &str { &self.name }
        fn invoke(&self, _phase: Phase, _op: Operation, context: &str) -> Result<Verdict, HookError> {
            let modified = format!("{}{}", context, self.suffix);
            Ok(Verdict::Continue { modified_context: Some(modified) })
        }
    }

    let mut pipeline = HookPipeline::new();
    pipeline.add(Operation::ToolCall, Arc::new(ModifyHook { name: "a".into(), suffix: "+A".into() }));
    pipeline.add(Operation::ToolCall, Arc::new(ModifyHook { name: "b".into(), suffix: "+B".into() }));

    let result = pipeline.run_before(Operation::ToolCall, "start");
    let (_, final_ctx) = result.unwrap();
    assert_eq!(final_ctx, "start+A+B", "modifications should chain");
}

#[test]
fn no_hooks_for_operation_passes_through() {
    let mut pipeline = HookPipeline::new();
    // Add hook for tool_call but query llm
    pipeline.add(Operation::ToolCall, Arc::new(TestHook::new(
        "tool-only",
        Verdict::Deny { reason: "no".into() },
        Verdict::continue_unchanged(),
    )));

    let result = pipeline.run_before(Operation::Llm, "{}");
    assert!(result.is_ok(), "operation with no hooks should pass through");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p simulacra-hooks`
Expected: FAIL — `HookPipeline` doesn't exist

- [ ] **Step 3: Implement HookPipeline**

Create `crates/simulacra-hooks/src/pipeline.rs`:

```rust
use crate::{HookError, HookModule, Operation, Phase, Verdict};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use std::collections::HashMap;
use std::sync::Arc;

/// Lazily-initialized OTel meters for hooks.
struct HookMeters {
    invocations: Counter<u64>,
    denials: Counter<u64>,
    timeouts: Counter<u64>,
}

impl HookMeters {
    fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<HookMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-hooks");
            HookMeters {
                invocations: meter
                    .u64_counter("simulacra.hooks.invocations")
                    .with_description("Hook invocations")
                    .build(),
                denials: meter
                    .u64_counter("simulacra.hooks.denials")
                    .with_description("Hook denials (deny + kill)")
                    .build(),
                timeouts: meter
                    .u64_counter("simulacra.hooks.timeouts")
                    .with_description("Hook timeouts")
                    .build(),
            }
        })
    }
}

/// Ordered chain of hooks for a single operation type.
pub struct HookChain {
    hooks: Vec<Arc<dyn HookModule>>,
}

impl HookChain {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub fn add(&mut self, hook: Arc<dyn HookModule>) {
        self.hooks.push(hook);
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

/// All hook chains, keyed by operation type.
///
/// Created at bootstrap, shared via `Arc<HookPipeline>` to all
/// integration points (ToolRegistry, AgentLoop, HttpClient, SpawnAgentTool).
pub struct HookPipeline {
    chains: HashMap<Operation, HookChain>,
}

impl HookPipeline {
    pub fn new() -> Self {
        Self {
            chains: HashMap::new(),
        }
    }

    /// Add a hook to the chain for a given operation type.
    pub fn add(&mut self, operation: Operation, hook: Arc<dyn HookModule>) {
        self.chains
            .entry(operation)
            .or_insert_with(HookChain::new)
            .add(hook);
    }

    /// Run before-hooks in config order. Returns the (possibly modified) context.
    ///
    /// First Deny or Kill stops the chain and returns HookError.
    pub fn run_before(
        &self,
        operation: Operation,
        context: &str,
    ) -> Result<(Verdict, String), HookError> {
        let chain = match self.chains.get(&operation) {
            Some(c) if !c.is_empty() => c,
            _ => return Ok((Verdict::continue_unchanged(), context.to_string())),
        };

        let meters = HookMeters::get();
        let mut current_context = context.to_string();

        for hook in &chain.hooks {
            let _span = tracing::debug_span!(
                "simulacra_hook_invoke",
                simulacra.hook.name = hook.name(),
                simulacra.hook.operation = %operation,
                simulacra.hook.phase = "before",
                simulacra.hook.verdict = tracing::field::Empty,
            );

            meters.invocations.add(1, &[
                KeyValue::new("hook", hook.name().to_string()),
                KeyValue::new("operation", operation.to_string()),
                KeyValue::new("phase", "before"),
            ]);

            let verdict = hook.invoke(Phase::Before, operation, &current_context)
                .map_err(|e| {
                    meters.timeouts.add(1, &[
                        KeyValue::new("hook", hook.name().to_string()),
                        KeyValue::new("operation", operation.to_string()),
                    ]);
                    e
                })?;

            match &verdict {
                Verdict::Continue { modified_context } => {
                    if let Some(modified) = modified_context {
                        current_context = modified.clone();
                    }
                }
                Verdict::Deny { reason } => {
                    tracing::info!(
                        hook = hook.name(),
                        operation = %operation,
                        reason = %reason,
                        "hook denied operation"
                    );
                    meters.denials.add(1, &[
                        KeyValue::new("hook", hook.name().to_string()),
                        KeyValue::new("operation", operation.to_string()),
                    ]);
                    return Err(HookError::Denied {
                        hook: hook.name().to_string(),
                        reason: reason.clone(),
                    });
                }
                Verdict::Kill { reason } => {
                    tracing::info!(
                        hook = hook.name(),
                        operation = %operation,
                        reason = %reason,
                        "hook killed agent"
                    );
                    meters.denials.add(1, &[
                        KeyValue::new("hook", hook.name().to_string()),
                        KeyValue::new("operation", operation.to_string()),
                    ]);
                    return Err(HookError::Killed {
                        hook: hook.name().to_string(),
                        reason: reason.clone(),
                    });
                }
            }
        }

        Ok((Verdict::continue_unchanged(), current_context))
    }

    /// Run after-hooks in reverse config order. Returns the (possibly modified) context.
    ///
    /// Deny in after-phase is logged and ignored. Kill is enforced.
    pub fn run_after(
        &self,
        operation: Operation,
        context: &str,
    ) -> Result<(Verdict, String), HookError> {
        let chain = match self.chains.get(&operation) {
            Some(c) if !c.is_empty() => c,
            _ => return Ok((Verdict::continue_unchanged(), context.to_string())),
        };

        let meters = HookMeters::get();
        let mut current_context = context.to_string();

        // Reverse order for after-phase (onion unwinding)
        for hook in chain.hooks.iter().rev() {
            let _span = tracing::debug_span!(
                "simulacra_hook_invoke",
                simulacra.hook.name = hook.name(),
                simulacra.hook.operation = %operation,
                simulacra.hook.phase = "after",
                simulacra.hook.verdict = tracing::field::Empty,
            );

            meters.invocations.add(1, &[
                KeyValue::new("hook", hook.name().to_string()),
                KeyValue::new("operation", operation.to_string()),
                KeyValue::new("phase", "after"),
            ]);

            let verdict = hook.invoke(Phase::After, operation, &current_context)
                .map_err(|e| {
                    meters.timeouts.add(1, &[
                        KeyValue::new("hook", hook.name().to_string()),
                        KeyValue::new("operation", operation.to_string()),
                    ]);
                    e
                })?;

            match &verdict {
                Verdict::Continue { modified_context } => {
                    if let Some(modified) = modified_context {
                        current_context = modified.clone();
                    }
                }
                Verdict::Deny { reason } => {
                    // After-phase deny is logged but not enforced
                    tracing::warn!(
                        hook = hook.name(),
                        operation = %operation,
                        reason = %reason,
                        "hook denied in after-phase (ignored — operation already executed)"
                    );
                }
                Verdict::Kill { reason } => {
                    tracing::info!(
                        hook = hook.name(),
                        operation = %operation,
                        reason = %reason,
                        "hook killed agent in after-phase"
                    );
                    meters.denials.add(1, &[
                        KeyValue::new("hook", hook.name().to_string()),
                        KeyValue::new("operation", operation.to_string()),
                    ]);
                    return Err(HookError::Killed {
                        hook: hook.name().to_string(),
                        reason: reason.clone(),
                    });
                }
            }
        }

        Ok((Verdict::continue_unchanged(), current_context))
    }
}

impl Default for HookPipeline {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Add `continue_unchanged` helper to Verdict**

In `verdict.rs`:

```rust
impl Verdict {
    pub fn continue_unchanged() -> Self {
        Verdict::Continue { modified_context: None }
    }
}
```

- [ ] **Step 5: Update `lib.rs` exports**

```rust
mod error;
mod pipeline;
mod verdict;

pub use error::HookError;
pub use pipeline::HookPipeline;
pub use verdict::{Operation, Phase, Verdict};

pub trait HookModule: Send + Sync {
    fn name(&self) -> &str;
    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError>;
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p simulacra-hooks`
Expected: PASS — all pipeline tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-hooks/
git commit -m "feat(hooks): HookPipeline with onion execution, first-deny-wins chaining [S026]"
```

---

### Task 3: JS hook runtime with timeout

**Files:**
- Create: `crates/simulacra-hooks/src/js.rs`
- Create: `crates/simulacra-hooks/fixtures/pass-through.js`
- Create: `crates/simulacra-hooks/fixtures/deny-tool.js`
- Create: `crates/simulacra-hooks/fixtures/modify-context.js`
- Create: `crates/simulacra-hooks/fixtures/slow-hook.js`
- Create: `crates/simulacra-hooks/tests/js_hook_tests.rs`
- Modify: `crates/simulacra-hooks/src/lib.rs`

- [ ] **Step 1: Create sample JS hook fixtures**

Create `crates/simulacra-hooks/fixtures/pass-through.js`:
```javascript
export function invoke(phase, operation, context) {
    return { continue: null };
}
```

Create `crates/simulacra-hooks/fixtures/deny-tool.js`:
```javascript
export function invoke(phase, operation, context) {
    if (phase === "before" && operation === "tool_call") {
        const ctx = JSON.parse(context);
        if (ctx.tool === "dangerous_tool") {
            return { deny: "dangerous_tool is not allowed" };
        }
    }
    return { continue: null };
}
```

Create `crates/simulacra-hooks/fixtures/modify-context.js`:
```javascript
export function invoke(phase, operation, context) {
    if (phase === "after" && operation === "tool_call") {
        const ctx = JSON.parse(context);
        // Redact any SSN-like patterns
        if (ctx.result && /\d{3}-\d{2}-\d{4}/.test(ctx.result)) {
            ctx.result = ctx.result.replace(/\d{3}-\d{2}-\d{4}/g, "***-**-****");
            return { continue: JSON.stringify(ctx) };
        }
    }
    return { continue: null };
}
```

Create `crates/simulacra-hooks/fixtures/slow-hook.js`:
```javascript
export function invoke(phase, operation, context) {
    // Spin until timeout
    const start = Date.now();
    while (Date.now() - start < 5000) {
        // busy wait
    }
    return { continue: null };
}
```

- [ ] **Step 2: Write failing tests**

Create `crates/simulacra-hooks/tests/js_hook_tests.rs`:

```rust
use simulacra_hooks::{HookError, HookModule, Operation, Phase, Verdict};
use simulacra_hooks::JsHookModule;

#[test]
fn js_pass_through_hook_returns_continue() {
    let hook = JsHookModule::from_file("pass-through", "fixtures/pass-through.js", 100)
        .expect("should load JS hook");
    let result = hook.invoke(Phase::Before, Operation::ToolCall, r#"{"tool":"echo"}"#);
    assert!(result.is_ok());
    assert!(matches!(result.unwrap(), Verdict::Continue { modified_context: None }));
}

#[test]
fn js_deny_hook_denies_dangerous_tool() {
    let hook = JsHookModule::from_file("denier", "fixtures/deny-tool.js", 100)
        .expect("should load JS hook");

    // Allowed tool
    let result = hook.invoke(Phase::Before, Operation::ToolCall, r#"{"tool":"echo"}"#);
    assert!(matches!(result.unwrap(), Verdict::Continue { .. }));

    // Dangerous tool
    let result = hook.invoke(Phase::Before, Operation::ToolCall, r#"{"tool":"dangerous_tool"}"#);
    assert!(matches!(result.unwrap(), Verdict::Deny { reason } if reason.contains("dangerous_tool")));
}

#[test]
fn js_modify_hook_redacts_ssn() {
    let hook = JsHookModule::from_file("modifier", "fixtures/modify-context.js", 100)
        .expect("should load JS hook");

    let ctx = r#"{"tool":"query","result":"SSN is 123-45-6789"}"#;
    let result = hook.invoke(Phase::After, Operation::ToolCall, ctx);
    let verdict = result.unwrap();
    match verdict {
        Verdict::Continue { modified_context: Some(modified) } => {
            assert!(modified.contains("***-**-****"), "SSN should be redacted: {modified}");
            assert!(!modified.contains("123-45-6789"), "original SSN should not appear");
        }
        _ => panic!("expected Continue with modification, got {:?}", verdict),
    }
}

#[test]
fn js_hook_timeout_returns_deny() {
    let hook = JsHookModule::from_file("slow", "fixtures/slow-hook.js", 50) // 50ms timeout
        .expect("should load JS hook");

    let start = std::time::Instant::now();
    let result = hook.invoke(Phase::Before, Operation::ToolCall, "{}");
    let elapsed = start.elapsed();

    // Should fail within reasonable time (not 5 seconds)
    assert!(elapsed.as_millis() < 1000, "timeout should fire, took {:?}", elapsed);
    assert!(matches!(result, Err(HookError::Timeout { .. }) | Err(HookError::Denied { .. })),
        "timeout should produce deny/timeout error: {:?}", result);
}

#[test]
fn js_hook_fresh_runtime_per_invocation() {
    // A hook that tries to set a global variable
    let script = r#"
        let counter = 0;
        export function invoke(phase, operation, context) {
            counter += 1;
            return { continue: JSON.stringify({ count: counter }) };
        }
    "#;
    let hook = JsHookModule::from_source("counter", script, 100)
        .expect("should create JS hook");

    let r1 = hook.invoke(Phase::Before, Operation::ToolCall, "{}").unwrap();
    let r2 = hook.invoke(Phase::Before, Operation::ToolCall, "{}").unwrap();

    // Both should return count=1 (fresh runtime each time)
    match (r1, r2) {
        (Verdict::Continue { modified_context: Some(c1) }, Verdict::Continue { modified_context: Some(c2) }) => {
            assert!(c1.contains("1"), "first call count should be 1: {c1}");
            assert!(c2.contains("1"), "second call count should also be 1 (fresh runtime): {c2}");
        }
        _ => panic!("expected Continue with modification"),
    }
}

#[test]
fn js_hook_invalid_return_value_returns_error() {
    let script = r#"
        export function invoke(phase, operation, context) {
            return "not a valid verdict object";
        }
    "#;
    let hook = JsHookModule::from_source("bad", script, 100)
        .expect("should create JS hook");

    let result = hook.invoke(Phase::Before, Operation::ToolCall, "{}");
    assert!(matches!(result, Err(HookError::ExecutionError { .. })));
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p simulacra-hooks`
Expected: FAIL — `JsHookModule` doesn't exist

- [ ] **Step 4: Implement JsHookModule**

Create `crates/simulacra-hooks/src/js.rs`:

```rust
use crate::{HookError, HookModule, Operation, Phase, Verdict};
use std::path::Path;

/// A governance hook implemented in JavaScript, evaluated via rquickjs.
///
/// Each `invoke` call creates a fresh QuickJS runtime for isolation.
/// The JS module must export an `invoke(phase, operation, context)` function
/// returning a verdict object.
pub struct JsHookModule {
    hook_name: String,
    script: String,
    timeout_ms: u64,
}

impl JsHookModule {
    /// Create from a JS file path.
    pub fn from_file(name: &str, path: impl AsRef<Path>, timeout_ms: u64) -> Result<Self, HookError> {
        let script = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            HookError::ExecutionError {
                hook: name.to_string(),
                source: format!("failed to read {}: {e}", path.as_ref().display()),
            }
        })?;
        Ok(Self {
            hook_name: name.to_string(),
            script,
            timeout_ms,
        })
    }

    /// Create from inline JS source.
    pub fn from_source(name: &str, script: &str, timeout_ms: u64) -> Result<Self, HookError> {
        Ok(Self {
            hook_name: name.to_string(),
            script: script.to_string(),
            timeout_ms,
        })
    }

    /// Parse a JS return value into a Verdict.
    fn parse_verdict(value: &rquickjs::Value<'_>) -> Result<Verdict, String> {
        let obj = value.as_object().ok_or("hook must return an object")?;

        // Check for { continue: null | "json" }
        if let Ok(val) = obj.get::<_, rquickjs::Value>("continue") {
            if val.is_null() || val.is_undefined() {
                return Ok(Verdict::continue_unchanged());
            }
            if let Some(s) = val.as_string() {
                let modified = s.to_string().map_err(|e| format!("continue string: {e}"))?;
                return Ok(Verdict::Continue {
                    modified_context: Some(modified),
                });
            }
            return Err("continue value must be null or string".into());
        }

        // Check for { deny: "reason" }
        if let Ok(val) = obj.get::<_, rquickjs::Value>("deny") {
            if let Some(s) = val.as_string() {
                let reason = s.to_string().map_err(|e| format!("deny string: {e}"))?;
                return Ok(Verdict::Deny { reason });
            }
            return Err("deny value must be a string".into());
        }

        // Check for { kill: "reason" }
        if let Ok(val) = obj.get::<_, rquickjs::Value>("kill") {
            if let Some(s) = val.as_string() {
                let reason = s.to_string().map_err(|e| format!("kill string: {e}"))?;
                return Ok(Verdict::Kill { reason });
            }
            return Err("kill value must be a string".into());
        }

        Err("hook must return object with 'continue', 'deny', or 'kill' key".into())
    }
}

impl HookModule for JsHookModule {
    fn name(&self) -> &str {
        &self.hook_name
    }

    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError> {
        let phase_str = phase.to_string();
        let op_str = operation.to_string();
        let context_owned = context.to_string();
        let script = self.script.clone();
        let hook_name = self.hook_name.clone();
        let timeout = std::time::Duration::from_millis(self.timeout_ms);

        // Run in a fresh rquickjs Runtime
        let rt = rquickjs::Runtime::new().map_err(|e| HookError::ExecutionError {
            hook: hook_name.clone(),
            source: format!("runtime creation: {e}"),
        })?;

        // Set interrupt handler for timeout
        let deadline = std::time::Instant::now() + timeout;
        rt.set_interrupt_handler(Some(Box::new(move || {
            std::time::Instant::now() > deadline
        })));

        let ctx = rquickjs::Context::full(&rt).map_err(|e| HookError::ExecutionError {
            hook: hook_name.clone(),
            source: format!("context creation: {e}"),
        })?;

        ctx.with(|ctx| {
            // Evaluate the script as a module to support `export function invoke`
            // For simplicity, wrap it as a script that defines invoke globally
            let wrapped = format!(
                r#"
                var __module = (function() {{
                    {script}
                    return {{ invoke }};
                }})();
                "#
            );

            ctx.eval::<(), _>(wrapped.as_bytes()).map_err(|e| {
                if format!("{e}").contains("interrupted") {
                    return HookError::Timeout {
                        hook: hook_name.clone(),
                        timeout_ms: self.timeout_ms,
                    };
                }
                HookError::ExecutionError {
                    hook: hook_name.clone(),
                    source: format!("eval: {e}"),
                }
            })?;

            // Call __module.invoke(phase, operation, context)
            let global = ctx.globals();
            let module: rquickjs::Object = global.get("__module").map_err(|e| {
                HookError::ExecutionError {
                    hook: hook_name.clone(),
                    source: format!("missing __module: {e}"),
                }
            })?;

            let invoke_fn: rquickjs::Function = module.get("invoke").map_err(|e| {
                HookError::ExecutionError {
                    hook: hook_name.clone(),
                    source: format!("missing invoke function: {e}"),
                }
            })?;

            let result: rquickjs::Value = invoke_fn
                .call((phase_str.as_str(), op_str.as_str(), context_owned.as_str()))
                .map_err(|e| {
                    let msg = format!("{e}");
                    if msg.contains("interrupted") {
                        return HookError::Timeout {
                            hook: hook_name.clone(),
                            timeout_ms: self.timeout_ms,
                        };
                    }
                    HookError::ExecutionError {
                        hook: hook_name.clone(),
                        source: format!("invoke call: {e}"),
                    }
                })?;

            Self::parse_verdict(&result).map_err(|e| HookError::ExecutionError {
                hook: hook_name.clone(),
                source: e,
            })
        })
    }
}
```

Note: The exact rquickjs API (Runtime::new, Context::full, set_interrupt_handler, eval, call) depends on the version in the workspace. The implementer should read the existing `simulacra-quickjs` crate's usage of rquickjs for the exact patterns. The `set_interrupt_handler` method might be different — check the rquickjs docs. If interrupt handling isn't available, use a thread with a join timeout as a fallback.

- [ ] **Step 5: Export JsHookModule from lib.rs**

Add to `crates/simulacra-hooks/src/lib.rs`:

```rust
mod js;
pub use js::JsHookModule;
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p simulacra-hooks`
Expected: PASS — all pipeline and JS tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/simulacra-hooks/
git commit -m "feat(hooks): JS hook runtime with timeout enforcement [S026]"
```

---

### Task 4: Config types for hooks

**Files:**
- Modify: `crates/simulacra-config/src/lib.rs`

- [ ] **Step 1: Add config types**

In `crates/simulacra-config/src/lib.rs`, add:

```rust
/// Governance hooks configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub tool_call: Vec<HookEntry>,
    #[serde(default)]
    pub llm: Vec<HookEntry>,
    #[serde(default)]
    pub spawn: Vec<HookEntry>,
    #[serde(default)]
    pub http_request: Vec<HookEntry>,
}

/// A single hook entry in the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEntry {
    pub name: String,
    pub runtime: String,
    pub module: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout_ms: u64,
}

fn default_hook_timeout() -> u64 {
    100
}
```

Add `pub hooks: Option<HooksConfig>` to `SimulacraConfig`.

- [ ] **Step 2: Add config parsing test**

```rust
#[test]
fn hooks_config_parses() {
    let config: SimulacraConfig = toml::from_str(r#"
        [project]
        name = "test"

        [agent_types.default]
        model = "test-model"
        system_prompt = "test"

        [[hooks.tool_call]]
        name = "pii-scanner"
        runtime = "js"
        module = "hooks/scan-pii.js"
        timeout_ms = 200

        [[hooks.http_request]]
        name = "url-filter"
        runtime = "js"
        module = "hooks/url-filter.js"
    "#).expect("hooks config should parse");

    let hooks = config.hooks.expect("hooks section should exist");
    assert_eq!(hooks.tool_call.len(), 1);
    assert_eq!(hooks.tool_call[0].name, "pii-scanner");
    assert_eq!(hooks.tool_call[0].timeout_ms, 200);
    assert_eq!(hooks.http_request.len(), 1);
    assert_eq!(hooks.http_request[0].timeout_ms, 100); // default
    assert!(hooks.llm.is_empty());
    assert!(hooks.spawn.is_empty());
}
```

- [ ] **Step 3: Fix compilation across workspace**

Add `hooks: None` to any `SimulacraConfig` struct literal construction sites.

Run: `cargo build --workspace`
Run: `cargo test -p simulacra-config`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-config/src/lib.rs
git commit -m "feat(config): add [[hooks.*]] config sections for governance pipeline [S026]"
```

---

### Task 5: Integration — thread HookPipeline through ToolRegistry

**Files:**
- Modify: `crates/simulacra-tool/src/lib.rs`
- Modify: `crates/simulacra-tool/Cargo.toml`

The ToolRegistry wraps tool calls with the hook pipeline's `tool_call` chain.

- [ ] **Step 1: Add `simulacra-hooks` dependency to `simulacra-tool`**

In `crates/simulacra-tool/Cargo.toml`, add:
```toml
simulacra-hooks.workspace = true
```

- [ ] **Step 2: Add pipeline to ToolRegistry**

Modify `ToolRegistry`:

```rust
use simulacra_hooks::{HookPipeline, Operation, HookError};
use std::sync::Arc;

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    pipeline: Option<Arc<HookPipeline>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new(), pipeline: None }
    }

    pub fn with_pipeline(pipeline: Arc<HookPipeline>) -> Self {
        Self { tools: Vec::new(), pipeline: Some(pipeline) }
    }

    pub fn set_pipeline(&mut self, pipeline: Arc<HookPipeline>) {
        self.pipeline = Some(pipeline);
    }
    // ... existing register, definitions methods unchanged ...
}
```

- [ ] **Step 3: Wrap tool calls with pipeline**

Update `ToolRegistry::call()`:

```rust
pub fn call<'a>(
    &'a self,
    name: &'a str,
    arguments: serde_json::Value,
    capability: &'a CapabilityToken,
) -> impl std::future::Future<Output = Result<serde_json::Value, ToolError>> + 'a {
    let span = tracing::info_span!("tool_invoke", gen_ai.tool.name = name);

    async move {
        let _guard = span.enter();

        let tool = self
            .tools
            .iter()
            .find(|t| t.definition().name == name)
            .ok_or_else(|| ToolError::ExecutionFailed(format!("unknown tool: {name}")))?;

        // Before-hooks
        let effective_args = if let Some(ref pipeline) = self.pipeline {
            let context = serde_json::json!({
                "tool": name,
                "arguments": &arguments,
            }).to_string();

            match pipeline.run_before(Operation::ToolCall, &context) {
                Ok((_, modified_ctx)) => {
                    // Parse modified context to extract possibly-changed arguments
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&modified_ctx) {
                        parsed.get("arguments").cloned().unwrap_or(arguments.clone())
                    } else {
                        arguments.clone()
                    }
                }
                Err(HookError::Denied { hook, reason }) => {
                    return Err(ToolError::ExecutionFailed(
                        format!("hook {hook} denied: {reason}")
                    ));
                }
                Err(HookError::Killed { hook, reason }) => {
                    return Err(ToolError::ExecutionFailed(
                        format!("KILL: hook {hook} killed agent: {reason}")
                    ));
                }
                Err(e) => {
                    return Err(ToolError::ExecutionFailed(format!("hook error: {e}")));
                }
            }
        } else {
            arguments.clone()
        };

        // Execute the tool
        let result = tool.call(effective_args, capability).await;

        // After-hooks
        match &result {
            Ok(value) => {
                if let Some(ref pipeline) = self.pipeline {
                    let context = serde_json::json!({
                        "tool": name,
                        "result": value,
                    }).to_string();

                    match pipeline.run_after(Operation::ToolCall, &context) {
                        Ok((_, modified_ctx)) => {
                            // Parse modified result
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&modified_ctx) {
                                if let Some(modified_result) = parsed.get("result") {
                                    let result_str = modified_result.to_string();
                                    tracing::info!(
                                        gen_ai.tool.message = %result_str,
                                        gen_ai.tool.name = name,
                                        "tool result"
                                    );
                                    return Ok(modified_result.clone());
                                }
                            }
                        }
                        Err(HookError::Killed { hook, reason }) => {
                            return Err(ToolError::ExecutionFailed(
                                format!("KILL: hook {hook} killed agent: {reason}")
                            ));
                        }
                        Err(_) => {
                            // Other after-hook errors are non-fatal
                        }
                    }
                }

                let result_str = value.to_string();
                tracing::info!(
                    gen_ai.tool.message = %result_str,
                    gen_ai.tool.name = name,
                    "tool result"
                );
                Ok(value.clone())
            }
            Err(err) => {
                tracing::error!(
                    gen_ai.tool.name = name,
                    error = %err,
                    "tool error"
                );
                result
            }
        }
    }
}
```

- [ ] **Step 4: Fix compilation and tests**

Run: `cargo build --workspace`
Run: `cargo test -p simulacra-tool`
Expected: PASS — existing tests use `ToolRegistry::new()` which has no pipeline (None)

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-tool/
git commit -m "feat(tool): wrap tool calls with governance hook pipeline [S026]"
```

---

### Task 6: Integration — wrap LLM calls in agent loop

**Files:**
- Modify: `crates/simulacra-runtime/Cargo.toml`
- Modify: `crates/simulacra-runtime/src/agent_loop.rs`

- [ ] **Step 1: Add `simulacra-hooks` dependency**

In `crates/simulacra-runtime/Cargo.toml`, add `simulacra-hooks.workspace = true`.

- [ ] **Step 2: Add pipeline to AgentLoop**

Add `pipeline: Option<Arc<HookPipeline>>` field to `AgentLoop`. Update `AgentLoop::new()` to accept it. Pass `None` in existing tests to avoid breaking them.

- [ ] **Step 3: Wrap provider.chat() with llm hooks**

At the two `provider.chat()` call sites (around lines 249 and 501), add before/after hook calls:

```rust
// Before LLM call
if let Some(ref pipeline) = self.pipeline {
    let context = serde_json::json!({
        "model": &self.config.model,
        "message_count": compacted.len(),
    }).to_string();

    if let Err(e) = pipeline.run_before(Operation::Llm, &context) {
        // Handle deny/kill
    }
}

// ... provider.chat() ...

// After LLM call
if let Some(ref pipeline) = self.pipeline {
    let context = serde_json::json!({
        "model": &self.config.model,
        "content": &response_text,
        "usage": { /* token counts */ },
    }).to_string();

    if let Err(HookError::Killed { .. }) = pipeline.run_after(Operation::Llm, &context) {
        // Kill the agent
    }
}
```

The implementer should read the exact agent loop structure to find where `response_text` is available and place the after-hook appropriately.

- [ ] **Step 4: Fix compilation and tests**

All existing `AgentLoop::new()` calls need the new pipeline parameter (pass `None`).

Run: `cargo build --workspace`
Run: `cargo test -p simulacra-runtime`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/simulacra-runtime/
git commit -m "feat(runtime): wrap LLM calls with governance hook pipeline [S026]"
```

---

### Task 7: Integration — wrap HTTP requests and spawn

**Files:**
- Modify: `crates/simulacra-http/Cargo.toml`
- Modify: `crates/simulacra-http/src/lib.rs`
- Modify: `crates/simulacra-http/src/client.rs`
- Modify: `crates/simulacra-runtime/src/spawn_tool.rs`

- [ ] **Step 1: Add pipeline to HttpClient**

In `crates/simulacra-http/Cargo.toml`, add `simulacra-hooks.workspace = true`.

Modify `UreqHttpClient` to accept an optional `Arc<HookPipeline>`:

```rust
pub struct UreqHttpClient {
    timeout_ms: u64,
    max_redirects: u32,
    pipeline: Option<Arc<HookPipeline>>,
}
```

Add constructor: `pub fn with_pipeline(timeout_ms, max_redirects, pipeline) -> Self`.

Wrap `execute()`:

```rust
fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
    // Before-hook
    if let Some(ref pipeline) = self.pipeline {
        let context = serde_json::json!({
            "url": &request.url,
            "method": &request.method,
            "headers": &request.headers,
            "body": &request.body,
        }).to_string();

        match pipeline.run_before(Operation::HttpRequest, &context) {
            Err(HookError::Denied { reason, .. }) => {
                return Err(HttpError::Network(format!("hook denied: {reason}")));
            }
            Err(HookError::Killed { reason, .. }) => {
                return Err(HttpError::Network(format!("KILL: {reason}")));
            }
            _ => {}
        }
    }

    // ... existing execute logic ...

    // After-hook (on success)
    if let Some(ref pipeline) = self.pipeline {
        let context = serde_json::json!({
            "url": &request.url,
            "method": &request.method,
            "status": response.status,
            "headers": &response.headers,
            "body": &response.body,
        }).to_string();

        let _ = pipeline.run_after(Operation::HttpRequest, &context);
        // After-hook errors are non-fatal for HTTP (Kill would need to propagate differently)
    }

    Ok(response)
}
```

- [ ] **Step 2: Wrap spawn in SpawnAgentTool**

In `crates/simulacra-runtime/src/spawn_tool.rs`, wrap the child agent creation with `spawn` hooks. The before-hook runs before creating the child loop, the after-hook runs after the child completes.

The implementer should read the spawn_tool.rs code to find the right insertion points — there are two paths (configured and generic).

- [ ] **Step 3: Fix compilation and tests**

All `UreqHttpClient::new()` calls may need updating. Existing tests should use `None` for pipeline.

Run: `cargo build --workspace`
Run: `cargo test --workspace`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/simulacra-http/ crates/simulacra-runtime/src/spawn_tool.rs
git commit -m "feat(hooks): wrap HTTP requests and spawn with governance pipeline [S026]"
```

---

### Task 8: CLI bootstrap wiring and journal integration

**Files:**
- Modify: `crates/simulacra-cli/Cargo.toml`
- Modify: `crates/simulacra-cli/src/lib.rs`
- Modify: `crates/simulacra-types/src/journal.rs`

- [ ] **Step 1: Add journal entry kinds**

In `crates/simulacra-types/src/journal.rs`, add to `JournalEntryKind`:

```rust
HookDenial {
    hook_name: String,
    operation: String,
    reason: String,
},
HookKill {
    hook_name: String,
    operation: String,
    reason: String,
},
```

- [ ] **Step 2: Add `simulacra-hooks` dependency to CLI**

In `crates/simulacra-cli/Cargo.toml`:
```toml
simulacra-hooks.workspace = true
```

- [ ] **Step 3: Build HookPipeline in bootstrap**

In `crates/simulacra-cli/src/lib.rs`, in `bootstrap()`, after config parsing and before tool registration:

```rust
use simulacra_hooks::{HookPipeline, JsHookModule, Operation};
use std::sync::Arc;

// Build governance hook pipeline from config
let pipeline = Arc::new({
    let mut pipeline = HookPipeline::new();

    if let Some(ref hooks_config) = config.hooks {
        let operation_chains = [
            (Operation::ToolCall, &hooks_config.tool_call),
            (Operation::Llm, &hooks_config.llm),
            (Operation::Spawn, &hooks_config.spawn),
            (Operation::HttpRequest, &hooks_config.http_request),
        ];

        for (operation, entries) in &operation_chains {
            for entry in *entries {
                match entry.runtime.as_str() {
                    "js" => {
                        match JsHookModule::from_file(&entry.name, &entry.module, entry.timeout_ms) {
                            Ok(hook) => {
                                pipeline.add(*operation, Arc::new(hook));
                                tracing::info!(
                                    hook = %entry.name,
                                    operation = %operation,
                                    "governance hook registered"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    hook = %entry.name,
                                    error = %e,
                                    "failed to load governance hook"
                                );
                            }
                        }
                    }
                    other => {
                        tracing::warn!(
                            hook = %entry.name,
                            runtime = other,
                            "unsupported hook runtime (only 'js' is supported)"
                        );
                    }
                }
            }
        }
    }

    pipeline
});

// Pass pipeline to ToolRegistry
let mut registry = ToolRegistry::with_pipeline(Arc::clone(&pipeline));
// ... existing tool registration ...

// Pass pipeline to AgentLoop (via the constructor)
// Pass pipeline to UreqHttpClient (via constructor)
```

- [ ] **Step 4: Thread pipeline through to all consumers**

Update the `AgentLoop::new()` call in bootstrap to pass `Some(Arc::clone(&pipeline))`.
Update `UreqHttpClient` creation to pass `Some(Arc::clone(&pipeline))`.

- [ ] **Step 5: Mechanical gate**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- [ ] **Step 6: Commit**

```bash
git add crates/simulacra-cli/ crates/simulacra-types/src/journal.rs
git commit -m "feat(cli): wire governance hook pipeline at bootstrap [S026]"
```

---

## Self-Review

**Spec coverage:**

| Spec Section | Task |
|---|---|
| Pipeline execution (behaviors 1-10) | Task 2 |
| JS hook execution (behaviors 11-18) | Task 3 |
| Timeout enforcement (behaviors 19-23) | Task 3 |
| ToolRegistry integration (behavior 24) | Task 5 |
| AgentLoop/LLM integration (behavior 25) | Task 6 |
| Spawn integration (behavior 26) | Task 7 |
| HttpRequest integration (behavior 27) | Task 7 |
| Pipeline sharing (behavior 28) | Task 8 |
| Kill handling (behavior 29) | Tasks 5, 6 |
| Journal (behaviors 30-31) | Task 8 |
| Config (behaviors 32-36) | Task 4 |
| Observability | Task 2 (meters in pipeline) |

**Placeholder scan:** No TBD/TODO. JS hook implementation has a note about rquickjs API variations — implementer should check existing simulacra-quickjs for patterns.

**Type consistency:**
- `Verdict` / `Phase` / `Operation`: defined Task 1, used everywhere
- `HookPipeline`: defined Task 2, threaded through Tasks 5-8
- `JsHookModule`: defined Task 3, instantiated in Task 8
- `HooksConfig` / `HookEntry`: defined Task 4, consumed in Task 8
- `HookError`: defined Task 1, matched in Tasks 5-7
