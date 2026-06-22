//! Integration fabric for Simulacra — credential lifecycle, service discovery, and credential injection.

pub mod injector;
pub mod metrics;
pub mod registry;
pub mod types;

pub use injector::*;
pub use registry::*;
pub use types::*;
