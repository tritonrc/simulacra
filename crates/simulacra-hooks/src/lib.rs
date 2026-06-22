pub mod error;
pub mod js;
pub mod pipeline;
pub mod verdict;

pub use error::HookError;
pub use pipeline::HookPipeline;
pub use verdict::{Operation, Phase, Verdict};

/// A hook module that can intercept operations.
pub trait HookModule: Send + Sync {
    /// The name of this hook (used for attribution in errors and logging).
    fn name(&self) -> &str;

    /// Invoke the hook for a given phase, operation, and context JSON.
    fn invoke(
        &self,
        phase: Phase,
        operation: Operation,
        context: &str,
    ) -> Result<Verdict, HookError>;
}
