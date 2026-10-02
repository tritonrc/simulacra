//! Integration fabric for Simulacra — credential lifecycle, service discovery, and credential injection.

// `#[async_trait]` emits an explicit `#[must_use]` on the desugared method;
// the `Result` it already returns is must_use too, which newer clippy flags
// as a redundant doubling. The doubling is in the macro expansion, not in
// any trait here.
#![allow(clippy::double_must_use)]

pub mod injector;
pub mod metrics;
pub mod registry;
pub mod types;

pub use injector::*;
pub use registry::*;
pub use types::*;
