//! HTTP control plane for Simulacra.
//!
//! Provides a shared [`HttpClient`] trait and a [`UreqHttpClient`] implementation
//! backed by [ureq](https://crates.io/crates/ureq). This crate contains no
//! agent, sandbox, or JS concepts — it is the governed surface that all HTTP
//! paths in the system converge on.

mod client;
mod types;

pub use client::UreqHttpClient;
pub use types::{HttpError, HttpRequest, HttpResponse};

/// Trait for executing HTTP requests.
///
/// Implementations must be `Send + Sync` so they can be shared across threads.
pub trait HttpClient: Send + Sync {
    /// Execute an HTTP request and return the response.
    fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError>;
}
