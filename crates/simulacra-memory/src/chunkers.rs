//! Concrete `Chunker` implementations for the MVP.
//!
//! - [`MarkdownSectionChunker`] — splits Markdown on `#`/`##`/`###` headings.
//! - [`FixedTokenChunker`] — fixed-size whitespace-token windows with overlap.
//! - [`JsonlChunker`] — one chunk per non-empty line.
//!
//! See `specs/S037-memory-and-semantic-retrieval.md` §12.

use simulacra_types::Locator;

use crate::chunker::{Chunk, Chunker, FixedTokenChunker, JsonlChunker, MarkdownSectionChunker};
use crate::error::MemoryError;

fn decode_utf8(content: &[u8]) -> Result<&str, MemoryError> {
    std::str::from_utf8(content)
        .map_err(|_| MemoryError::Internal("invalid utf-8 in chunker source".to_string()))
}

/// Returns true if the line is a Markdown ATX heading at level 1, 2, or 3
/// (i.e. starts with `#`, `##`, or `###` followed by a space).
fn is_md_heading(line: &str) -> bool {
    let mut count = 0usize;
    for b in line.bytes() {
        if b == b'#' {
            count += 1;
            if count > 3 {
                return false;
            }
        } else {
            // Need at least one '#' followed by a space.
            return (1..=3).contains(&count) && b == b' ';
        }
    }
    false
}

// ─── MarkdownSectionChunker ───────────────────────────────────────────────────

impl Chunker for MarkdownSectionChunker {
    fn name(&self) -> &str {
        "markdown-section"
    }

    fn chunk(&self, _source_path: &str, content: &[u8]) -> Result<Vec<Chunk>, MemoryError> {
        if content.is_empty() {
            return Ok(Vec::new());
        }
        let source = decode_utf8(content)?;

        // Walk lines tracking byte offsets. Each heading line begins a new
        // section. Content before the first heading becomes an "intro" chunk.
        // If there are no headings, the whole document is one chunk.
        let mut section_starts: Vec<usize> = Vec::new();
        let mut byte = 0usize;
        for line in source.split_inclusive('\n') {
            // The line slice (without newline) — for heading detection we
            // strip the trailing '\n' if present.
            let line_no_nl = line.strip_suffix('\n').unwrap_or(line);
            if is_md_heading(line_no_nl) {
                section_starts.push(byte);
            }
            byte += line.len();
        }

        let mut chunks = Vec::new();

        if section_starts.is_empty() {
            chunks.push(Chunk {
                chunk_index: 0,
                locator: Locator::Text {
                    byte_start: 0,
                    byte_end: source.len(),
                },
                text: source.to_string(),
            });
            return Ok(chunks);
        }

        // Intro chunk: any content before the first heading.
        if section_starts[0] > 0 {
            let end = section_starts[0];
            chunks.push(Chunk {
                chunk_index: chunks.len(),
                locator: Locator::Text {
                    byte_start: 0,
                    byte_end: end,
                },
                text: source[..end].to_string(),
            });
        }

        for (i, &start) in section_starts.iter().enumerate() {
            let end = section_starts.get(i + 1).copied().unwrap_or(source.len());
            chunks.push(Chunk {
                chunk_index: chunks.len(),
                locator: Locator::Text {
                    byte_start: start,
                    byte_end: end,
                },
                text: source[start..end].to_string(),
            });
        }

        Ok(chunks)
    }
}

// ─── FixedTokenChunker ────────────────────────────────────────────────────────

/// A whitespace-delimited token with its byte range in the source.
#[derive(Debug, Clone, Copy)]
struct TokenSpan {
    byte_start: usize,
    byte_end: usize,
}

fn tokenize_with_spans(source: &str) -> Vec<TokenSpan> {
    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        // Skip whitespace. We rely on str::split_whitespace semantics, which
        // uses Unicode whitespace; but for byte-tracking we walk char_indices.
        // Use char-based scan to honor multi-byte whitespace correctly.
        // Simpler: iterate chars with their byte offsets.
        // To avoid re-parsing the prefix on every iteration we keep `i`.
        let rest = &source[i..];
        let mut chars = rest.char_indices();
        // Find start of next token.
        let token_start_rel = loop {
            match chars.next() {
                Some((off, c)) if c.is_whitespace() => {
                    let _ = off;
                    continue;
                }
                Some((off, _)) => break Some(off),
                None => break None,
            }
        };
        let Some(start_rel) = token_start_rel else {
            break;
        };
        // Find end of this token.
        let end_rel = loop {
            match chars.next() {
                Some((off, c)) if c.is_whitespace() => break off,
                Some((_, _)) => continue,
                None => break rest.len(),
            }
        };
        let start = i + start_rel;
        let end = i + end_rel;
        spans.push(TokenSpan {
            byte_start: start,
            byte_end: end,
        });
        i = end;
    }
    spans
}

impl Chunker for FixedTokenChunker {
    fn name(&self) -> &str {
        "fixed-token"
    }

    fn chunk(&self, _source_path: &str, content: &[u8]) -> Result<Vec<Chunk>, MemoryError> {
        if content.is_empty() {
            return Ok(Vec::new());
        }
        let source = decode_utf8(content)?;
        let spans = tokenize_with_spans(source);
        if spans.is_empty() {
            return Ok(Vec::new());
        }

        let window = self.tokens_per_chunk.max(1);
        let overlap = self.overlap_tokens.min(window.saturating_sub(1));
        let step = window - overlap;

        let mut chunks = Vec::new();
        let mut start_idx = 0usize;
        while start_idx < spans.len() {
            let end_idx = (start_idx + window).min(spans.len());
            let byte_start = spans[start_idx].byte_start;
            let byte_end = spans[end_idx - 1].byte_end;
            chunks.push(Chunk {
                chunk_index: chunks.len(),
                locator: Locator::Text {
                    byte_start,
                    byte_end,
                },
                text: source[byte_start..byte_end].to_string(),
            });
            if end_idx == spans.len() {
                break;
            }
            start_idx += step;
        }

        Ok(chunks)
    }
}

// ─── JsonlChunker ─────────────────────────────────────────────────────────────

impl Chunker for JsonlChunker {
    fn name(&self) -> &str {
        "jsonl-line"
    }

    fn chunk(&self, _source_path: &str, content: &[u8]) -> Result<Vec<Chunk>, MemoryError> {
        if content.is_empty() {
            return Ok(Vec::new());
        }
        let source = decode_utf8(content)?;

        let mut chunks = Vec::new();
        for (line_no, line) in source.split('\n').enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            chunks.push(Chunk {
                chunk_index: chunks.len(),
                locator: Locator::JsonlLine {
                    line: line_no as u64,
                },
                text: line.to_string(),
            });
        }

        Ok(chunks)
    }
}
