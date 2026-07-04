//! Shared test infrastructure for the `simulacra-sandbox` integration tests.
//!
//! Split into focused submodules so no single file exceeds the 500-line cap.
//! Everything is re-exported here so individual test files only need
//! `use common::*;`.

#![allow(dead_code)]
#![allow(unused_imports)]

pub use rust_decimal::Decimal;
pub use serde_json::Value;
pub use simulacra_quickjs::JsOutput;
pub use simulacra_sandbox::{AgentCell, ScriptExecutor};
pub use simulacra_shell::CommandResult;
pub use simulacra_types::{
    AgentId, CapabilityDenied, CapabilityToken, CheckpointData, FsMetadata, JOURNAL_SCHEMA_VERSION,
    JournalEntry, JournalEntryKind, JournalError, JournalStorage, NetworkPermission, PathPattern,
    ResourceBudget, TokenUsage, VfsError, VfsSnapshot, VirtualFs,
};
pub use simulacra_vfs::MemoryFs;
pub use std::collections::HashMap;
pub use std::io::{Read, Write};
pub use std::net::TcpListener;
pub use std::panic::{AssertUnwindSafe, catch_unwind};
pub use std::sync::{
    Arc, Barrier, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
pub use std::thread::{self, JoinHandle};
pub use std::time::Duration;
pub use tracing_subscriber::layer::SubscriberExt;

pub mod error;
pub mod fakes;
pub mod harness;
pub mod http_server;
pub mod tracing_capture;

pub use error::{ExpectedSandboxError, sandbox_error_to_expected};
pub use fakes::{FakeJournalStorage, PanicWriteFs, SlowWriteFs, SpyFs};
pub use harness::{
    Harness, MemoryHarness, assert_budget_exhausted, budget_counter, budget_with_overrides,
    capability, capability_token, capability_with_network, journal_payload, unlimited_budget,
};
pub use http_server::{TestHttpServer, reason_phrase, spawn_http_server};
pub use tracing_capture::{
    CaptureLayer, CapturedEvent, CapturedSpan, capture_operation, capture_spans, setup_capture,
    span_operations,
};
