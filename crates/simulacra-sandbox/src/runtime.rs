//! Shared runtime pieces: OTel meters and the `Send`/`Sync` JS-runtime wrapper.

use opentelemetry::metrics::{Counter, Histogram};
use simulacra_quickjs::JsRuntime;
use std::sync::{Mutex, MutexGuard};

use crate::SandboxError;

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for the sandbox.
///
/// Created on first use so they pick up the global MeterProvider, which may
/// not be set at construction time.
pub(crate) struct SandboxMeters {
    pub shell_duration: Histogram<f64>,
    pub shell_requests: Counter<u64>,
    pub js_duration: Histogram<f64>,
    pub js_requests: Counter<u64>,
}

impl SandboxMeters {
    pub(crate) fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<SandboxMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-sandbox");
            SandboxMeters {
                shell_duration: meter
                    .f64_histogram("simulacra.sandbox.shell.duration")
                    .with_unit("ms")
                    .with_description("Shell command execution duration")
                    .build(),
                shell_requests: meter
                    .u64_counter("simulacra.sandbox.shell.requests")
                    .with_description("Total shell command executions")
                    .build(),
                js_duration: meter
                    .f64_histogram("simulacra.sandbox.js.duration")
                    .with_unit("ms")
                    .with_description("JavaScript execution duration")
                    .build(),
                js_requests: meter
                    .u64_counter("simulacra.sandbox.js.requests")
                    .with_description("Total JavaScript executions")
                    .build(),
            }
        })
    }
}

/// Wrapper around [`JsRuntime`] that implements `Send` and `Sync`.
///
/// QuickJS contexts are not `Send`/`Sync` because they contain `Rc` and raw
/// pointers. However, `AgentCell` is designed so that each cell is owned
/// exclusively by a single agent task. The `Sync` bound is required because
/// `Arc<AgentCell>` is used in `simulacra-tool`, but concurrent access to the
/// JS runtime never actually occurs — each tool invocation runs sequentially
/// on the owning task. The inner `Mutex` provides runtime protection against
/// accidental concurrent access.
pub(crate) struct SendableJsRuntime(pub(crate) Mutex<Option<JsRuntime>>);

// SAFETY: JsRuntime is !Send because rquickjs types contain Rc and raw pointers.
// However, AgentCell ensures the runtime is only ever used from one logical task.
// The Mutex serializes all access, so only one thread ever touches the runtime at
// a time. QuickJS has no thread-affinity requirement — it does not use thread-local
// storage or thread-pinned resources. The `!Send` bound on rquickjs types is a
// conservative Rust-side restriction due to internal `Rc`s.
unsafe impl Send for SendableJsRuntime {}
unsafe impl Sync for SendableJsRuntime {}

impl SendableJsRuntime {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Lock and return a guard to the inner option.
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Option<JsRuntime>>, SandboxError> {
        self.0
            .lock()
            .map_err(|e| SandboxError::Internal(format!("js runtime mutex poisoned: {e}")))
    }
}
