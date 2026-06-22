//! Simulacra memory subsystem — long-term agent memory unified with RAG.
//!
//! See `specs/S037-memory-and-semantic-retrieval.md` for the full design.
//!
//! ## Layers
//!
//! - **Storage layer** ([`MemoryStore`]) — durable `(tenant, path) → bytes`
//!   mapping with monotonic per-path versions. Source of truth for memory
//!   content.
//! - **Index layer** ([`VectorIndex`]) — chunked embedding store supporting
//!   top-K semantic search. Derived from the storage layer.
//! - **Embedder layer** ([`Embedder`]) — produces unit-normalized vectors
//!   for chunks. Pluggable; default is local-first.
//! - **Chunker layer** ([`Chunker`]) — splits source content into chunks
//!   with source-type-aware locators.
//!
//! Core types — `TenantId`, `MemoryPath`, `MemoryVersion`, `Locator`,
//! `HitId`, `MemoryCapability` — live in `simulacra-types` because
//! `MemoryCapability` is a field of `CapabilityToken`.

mod background;
mod chunker;
mod chunkers;
mod embedder;
mod embedders;
mod error;
mod hit_cache;
mod index;
mod metrics;
mod reindex_startup;
mod retention;
mod rrwb;
mod sqlite_index;
mod sqlite_store;
mod store;

pub use background::{
    BackgroundEmbedder, BackgroundEmbedderConfig, ChunkerSelector, DEFAULT_ENQUEUE_TIMEOUT_MS,
    DEFAULT_QUEUE_CAPACITY,
};
pub use chunker::{Chunk, Chunker, FixedTokenChunker, JsonlChunker, MarkdownSectionChunker};
pub use embedder::{Embedder, EmbedderId};
pub use embedders::DefaultEmbedder;
pub use error::MemoryError;
pub use hit_cache::{HitCacheEntry, HitIdCache};
pub use index::{
    BACKLOG_MAX_RETRIES, BacklogRow, IndexedChunk, SearchHit, ToolSearchHit, UpsertOutcome,
    VectorIndex,
};
pub use metrics::record_embedder_load_failure;
pub use reindex_startup::{OnModelChangePolicy, apply_policy};
pub use retention::{ReaperStats, RetentionReaper, RetentionReaperConfig, RetentionSubtree};
pub use rrwb::RecentWritesBuffer;
pub use sqlite_index::SqliteVectorIndex;
pub use sqlite_store::{BroadcastEventReceiver, SqliteMemoryStore};
pub use store::{MemoryEntry, MemoryEvent, MemoryEventReceiver, MemoryRecvOutcome, MemoryStore};
