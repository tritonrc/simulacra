//! WHATWG Fetch API types for Simulacra's QuickJS sandbox.
//!
//! Provides [`Headers`], [`Blob`], [`Request`], [`Response`],
//! `AbortController`/`AbortSignal`, and the [`fetch()`](register_fetch_global)
//! global function, along with registration functions that install
//! WHATWG-compliant classes into a QuickJS context.
//!
//! Use [`register_globals`] to install everything at once, or call individual
//! `register_*` functions for fine-grained control.

mod abort;
mod blob;
mod fetch;
mod headers;
mod request;
mod response;

pub use abort::*;
pub use blob::*;
pub use fetch::*;
pub use headers::*;
pub use request::*;
pub use response::*;
