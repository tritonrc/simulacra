//! Monty Python engine for Simulacra.
//!
//! Wraps the Monty interpreter (pydantic/monty) to provide sandboxed Python
//! execution with external function mediation through the Golden Rule.
//!
//! Core runtime types are re-exported from `simulacra-python-runtime`. This crate
//! adds `PyExecTool` and `AgentCellDispatcher` which depend on `simulacra-sandbox`.

pub use simulacra_python_runtime::*;

pub mod tool;
pub use tool::PyExecTool;

use opentelemetry::metrics::{Counter, Histogram};

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for the Python engine.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
pub struct PythonMeters {
    pub executions: Counter<u64>,
    pub execution_time: Histogram<f64>,
    pub resource_limit_exceeded: Counter<u64>,
    pub external_calls: Counter<u64>,
}

impl PythonMeters {
    pub fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<PythonMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-python");
            PythonMeters {
                executions: meter
                    .u64_counter("simulacra.python.executions")
                    .with_description("Total Python code executions")
                    .build(),
                execution_time: meter
                    .f64_histogram("simulacra.python.execution_time_ms")
                    .with_unit("ms")
                    .with_description("Python execution duration")
                    .build(),
                resource_limit_exceeded: meter
                    .u64_counter("simulacra.python.resource_limit_exceeded")
                    .with_description("Python executions that hit resource limits")
                    .build(),
                external_calls: meter
                    .u64_counter("simulacra.python.external_calls")
                    .with_description("External function calls from Python runtime")
                    .build(),
            }
        })
    }
}
