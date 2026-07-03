use std::time::Duration;

use rquickjs::Ctx;

/// Default execution timeout for JS evaluation (5 seconds).
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// Workflow orchestration may legitimately await long-running agent workers.
/// S052 cancellation, not the QuickJS wall-clock guard, is the control plane for
/// those host waits.
pub(crate) const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Host API surface installed into a fresh QuickJS context.
///
/// The default profile preserves the regular `JsRuntime` behavior. Embedders
/// that need a narrower surface, such as workflow orchestration, can select a
/// profile that disables host globals and module exposure while keeping normal
/// QuickJS language semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsHostApiProfile {
    pub console: bool,
    pub fs: bool,
    pub process: bool,
    pub fetch: bool,
    pub web_globals: bool,
    pub module_loader: bool,
    pub simulacra_modules: bool,
}

impl JsHostApiProfile {
    /// Full Simulacra JS host surface.
    pub const fn full() -> Self {
        Self {
            console: true,
            fs: true,
            process: true,
            fetch: true,
            web_globals: true,
            module_loader: true,
            simulacra_modules: true,
        }
    }

    /// Restricted host surface for workflow orchestration scripts.
    pub const fn workflow() -> Self {
        Self {
            console: false,
            fs: false,
            process: false,
            fetch: false,
            web_globals: false,
            module_loader: false,
            simulacra_modules: false,
        }
    }
}

impl Default for JsHostApiProfile {
    fn default() -> Self {
        Self::full()
    }
}

/// Remove QuickJS ambient APIs that are not deterministic enough for workflow
/// orchestration.
///
/// This function does not parse or reinterpret workflow JavaScript. It mutates
/// the host context before user code runs so the normal QuickJS evaluator sees
/// a restricted global surface.
pub fn install_workflow_api_restrictions(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    ctx.eval::<(), _>(
        r#"
        globalThis.console = undefined;
        globalThis.fs = undefined;
        globalThis.process = undefined;
        globalThis.fetch = undefined;
        globalThis.require = undefined;
        globalThis.performance = undefined;
        globalThis.Date = undefined;
        Math.random = undefined;
        "#,
    )
}

/// Output captured from a JS evaluation.
#[derive(Debug, Clone, Default)]
pub struct JsOutput {
    /// All text written via `console.log`, including trailing newlines.
    pub stdout: String,
    /// The stringified return value of the evaluated expression, if any.
    pub result: Option<String>,
    /// Exit code if `process.exit(code)` was called, otherwise `None`.
    pub exit_code: Option<i32>,
}

/// Errors from the QuickJS runtime.
#[derive(Debug, thiserror::Error)]
pub enum JsError {
    /// Error initialising or interacting with the QuickJS runtime.
    #[error("runtime error: {0}")]
    Runtime(String),
    /// An uncaught JS exception.
    #[error("execution error: {0}")]
    Execution(String),
}
