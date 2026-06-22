use std::time::Duration;

use monty::{LimitedTracker, MontyObject, MontyRun, PrintWriter, ResourceLimits};

use crate::dispatch::{ExternalDispatcher, execute_with_dispatch};
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
        let mut rl = ResourceLimits::new();
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
    /// return the result. Any external function call or OS call will error.
    pub fn execute_simple(&self, code: &str) -> Result<PythonOutput, PythonError> {
        let runner = MontyRun::new(code.to_owned(), "<py_exec>", vec![])
            .map_err(|e| PythonError::ParseError(crate::error::format_exception(&e)))?;

        let mut stdout = String::new();
        let limits = self.build_resource_limits();
        let tracker = LimitedTracker::new(limits);
        let print_writer = PrintWriter::Collect(&mut stdout);

        let result = runner
            .run(vec![], tracker, print_writer)
            .map_err(PythonError::from)?;

        Ok(PythonOutput {
            stdout,
            result: Some(result),
        })
    }

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

    /// Returns the configured resource limits.
    pub fn resource_limits(&self) -> &PythonResourceLimits {
        &self.limits
    }

    /// Build a LimitedTracker from the configured limits.
    pub fn build_tracker(&self) -> LimitedTracker {
        LimitedTracker::new(self.build_resource_limits())
    }
}
