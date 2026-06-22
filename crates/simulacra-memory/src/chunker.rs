//! `Chunker` trait — splits source content into chunks with source-type-aware
//! locators.
//!
//! See S037 §4 (Locator) and §12 (Chunking strategies).

use simulacra_types::Locator;

use crate::error::MemoryError;

/// A single chunk produced by a chunker.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Ordinal within the source document (0-indexed).
    pub chunk_index: usize,
    /// Source-type-aware coordinates for re-locating this chunk.
    pub locator: Locator,
    /// Extracted text. This is what gets embedded and what
    /// `memory_read_chunk` returns.
    pub text: String,
}

/// Splits source content into chunks. Implementations are selected by
/// content type at the background embedder layer.
pub trait Chunker: Send + Sync + 'static {
    /// Identifier for this chunker (e.g. `"markdown-section"`,
    /// `"fixed-token"`, `"jsonl-line"`).
    fn name(&self) -> &str;

    /// Chunk a source document. `source_path` is the canonical memory path
    /// of the source (used for locator construction); `content` is the raw
    /// bytes from the store.
    fn chunk(&self, source_path: &str, content: &[u8]) -> Result<Vec<Chunk>, MemoryError>;
}

// ─── Marker types for the MVP chunker implementations ─────────────────────────

/// Splits Markdown content on `#`/`##`/`###` headings, preserving section
/// boundaries. Emits `Locator::Text { byte_start, byte_end }` covering each
/// section's contents.
pub struct MarkdownSectionChunker;

/// Fallback chunker. Splits content into 400-token windows with 50-token
/// overlap. Token counting is whitespace-based for the MVP. Emits
/// `Locator::Text` covering each window.
pub struct FixedTokenChunker {
    pub tokens_per_chunk: usize,
    pub overlap_tokens: usize,
}

impl Default for FixedTokenChunker {
    fn default() -> Self {
        Self {
            tokens_per_chunk: 400,
            overlap_tokens: 50,
        }
    }
}

/// Emits one chunk per non-empty line. Used for `.jsonl`/`.ndjson` files
/// like conversation logs. Emits `Locator::JsonlLine { line }`.
pub struct JsonlChunker;
