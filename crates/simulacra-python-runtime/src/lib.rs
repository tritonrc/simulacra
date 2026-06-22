//! Core Monty Python runtime for Simulacra.
//!
//! This is a leaf crate with no dependency on `simulacra-sandbox`. It provides:
//! - `PythonRuntime` / `PythonResourceLimits` / `PythonOutput` — execution engine
//! - `ExternalDispatcher` trait and `execute_with_dispatch` — generic dispatch loop
//! - `PythonError` — error types
//! - `monty_to_json` / `json_to_monty` — MontyObject conversion

mod convert;
mod dispatch;
mod error;
mod runtime;

pub use convert::{json_to_monty, monty_to_json};
pub use dispatch::{ExternalDispatcher, execute_with_dispatch};
pub use error::{PythonError, format_exception};
pub use runtime::{PythonOutput, PythonResourceLimits, PythonRuntime};
