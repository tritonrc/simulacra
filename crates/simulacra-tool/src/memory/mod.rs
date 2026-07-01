//! Memory tools: `semantic_search` and `memory_read_chunk`.
//!
//! These tools implement the retrieval surface described in S037 §9. They are
//! opt-in per agent via [`simulacra_types::MemoryCapability`]. Registration is
//! handled by [`register_memory_tools`] — `register_builtins` does NOT register
//! them.
//!
//! See the spec for the full contract; the high-level flow for
//! `semantic_search` is:
//!
//! 1. Parse the `scope` argument as a [`MemoryPath`], rejecting traversal.
//! 2. Check the scope against `MemoryCapability.search_scopes`. A scope
//!    outside the grant returns `{hits: []}` (no error, to avoid leaking the
//!    shape of the grant).
//! 3. Embed the query and call [`VectorIndex::search`].
//! 4. Consult the per-run [`RecentWritesBuffer`] (if wired) and merge
//!    persistent + RRWB hits: RRWB first (strictly newer), deduped by path,
//!    persistent hits sorted by cosine score, truncated to `k`.
//! 5. Mint a [`HitId`] for each surviving hit via [`HitIdCache::mint`] and
//!    return a list of `ToolSearchHit` as JSON.
//!
//! For `memory_read_chunk`:
//!
//! 1. Resolve the `hit_id` in the [`HitIdCache`]. Missing/expired → 404.
//! 2. TOCTOU guard: check the current path version in the `MemoryStore`. If
//!    the path is gone → 410 `chunk_deleted`. If the current version is
//!    higher than the cached one → 410 `chunk_stale`.
//! 3. Fetch the chunk via [`VectorIndex::get_chunk`] and return the content.

mod read_chunk;
mod registration;
mod search;

pub use read_chunk::MemoryReadChunkTool;
pub use registration::{MemoryToolHandles, register_memory_tools};
pub use search::SemanticSearchTool;
