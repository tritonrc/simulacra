//! AWS Bedrock provider (Converse API).
//!
//! Re-exports the public [`BedrockProvider`]. Internal modules:
//! - [`sigv4`] — in-process AWS SigV4 request signing.
//! - [`api_types`] — Converse request/response mapping + stream accumulator.
//! - [`eventstream`] — binary AWS Event Stream frame decoder.
//! - [`client`] — the `Provider` / `StreamingProvider` implementation.

mod api_types;
mod client;
mod eventstream;
mod sigv4;

pub use client::BedrockProvider;
