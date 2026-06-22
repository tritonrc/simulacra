//! `RecentWritesBuffer` (RRWB) — per-run cache providing Guarantee 2
//! (read-your-writes within a single agent run) for S037.
//!
//! The RRWB is a per-agent-run in-process buffer that holds recently-written
//! memory content. On `semantic_search`, the tool layer consults the RRWB
//! and merges results with the persistent index, so writes made earlier in
//! the same run are visible to subsequent searches in the same run.
//!
//! See `specs/S037-memory-and-semantic-retrieval.md` §7 Guarantee 2.
//!
//! ## Scope and capacity
//!
//! - Owned by `AgentLoop` for the duration of one run. Dropped at run end.
//! - Fixed capacity of [`RRWB_MAX_ENTRIES`] entries (oldest-first eviction).
//! - Per-entry byte cap [`RRWB_MAX_BYTES_PER_ENTRY`]. Oversized writes are
//!   NOT buffered — they fall through to Guarantee 3 (eventual consistency
//!   via the persistent index).
//! - Total byte cap [`RRWB_MAX_TOTAL_BYTES`]. When adding an entry would
//!   exceed the cap, evict oldest entries until it fits.
//!
//! ## Matching and scoring — MVP and target
//!
//! **MVP (this file):** case-insensitive substring matching on the raw UTF-8
//! payload. Each hit's `cosine_score` field carries a synthetic relevance
//! score `1 - 1/(1 + match_count)` in `[0, 1)`. **This is NOT a real cosine
//! similarity** and is NOT numerically comparable to persistent
//! `VectorIndex::search` scores, despite sharing the field name. The tool
//! layer that merges RRWB hits with persistent hits MUST treat them as
//! distinct categories — see S037 §7 Guarantee 2 point 6 for the merge
//! policy.
//!
//! **Target (Wave C):** embed-on-query. `RecentWritesBuffer::new` will grow
//! to accept an `Arc<dyn Embedder>` + chunker selector, `search` becomes
//! `async` and embeds pending entries synchronously, and the score becomes a
//! true cosine in `[-1, 1]` — directly comparable to persistent hits. At
//! that point the tool layer can drop the category asymmetry and do a
//! unified sort on `cosine_score` descending.
//!
//! The MVP is sufficient for the virtual coworker demo and the freshness
//! tests but is tagged as a known deviation in the spec (§7) so it's not
//! accidentally treated as the final design.

use simulacra_types::{
    MemoryPath, MemoryVersion, RRWB_MAX_BYTES_PER_ENTRY, RRWB_MAX_ENTRIES, RRWB_MAX_TOTAL_BYTES,
};
use std::collections::VecDeque;

use crate::index::SearchHit;
use simulacra_types::Locator;

/// Maximum length of the snippet surfaced in a `SearchHit` from the RRWB.
const RRWB_SNIPPET_CHARS: usize = 240;

/// A single buffered write held in the [`RecentWritesBuffer`].
#[derive(Debug, Clone)]
struct BufferedWrite {
    path: MemoryPath,
    version: MemoryVersion,
    /// UTF-8 payload. Binary writes are rejected at `record` time and do
    /// not land here.
    text: String,
}

/// Per-agent-run buffer of recently-written memory content.
///
/// See module docs for the contract. Construct with [`RecentWritesBuffer::new`],
/// populate with [`RecentWritesBuffer::record`], consult with
/// [`RecentWritesBuffer::search`].
#[derive(Debug, Default)]
pub struct RecentWritesBuffer {
    entries: VecDeque<BufferedWrite>,
    total_bytes: usize,
}

impl RecentWritesBuffer {
    /// Create a new empty buffer.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(RRWB_MAX_ENTRIES),
            total_bytes: 0,
        }
    }

    /// Attempt to record a write into the buffer.
    ///
    /// Silently drops writes that are:
    /// - larger than [`RRWB_MAX_BYTES_PER_ENTRY`], OR
    /// - not valid UTF-8.
    ///
    /// A dropped write still succeeds at the store level — it just does not
    /// get the read-your-writes fast path and must rely on Guarantee 3
    /// (eventual consistency via the persistent index).
    ///
    /// If the write fits but pushes the buffer past [`RRWB_MAX_ENTRIES`] or
    /// [`RRWB_MAX_TOTAL_BYTES`], the oldest entries are evicted first.
    pub fn record(&mut self, path: MemoryPath, version: MemoryVersion, data: &[u8]) {
        if data.len() > RRWB_MAX_BYTES_PER_ENTRY {
            return;
        }
        let text = match std::str::from_utf8(data) {
            Ok(s) => s.to_string(),
            Err(_) => return,
        };
        let entry_bytes = text.len();

        // If the single entry is larger than the total cap, nothing we can do.
        if entry_bytes > RRWB_MAX_TOTAL_BYTES {
            return;
        }

        // Drop any existing entry for the same path — a rewrite supersedes
        // the previous buffered version. Keeps the buffer showing only the
        // freshest content for read-your-writes.
        if let Some(pos) = self.entries.iter().position(|e| e.path == path) {
            let old = self.entries.remove(pos).expect("position was just found");
            self.total_bytes = self.total_bytes.saturating_sub(old.text.len());
        }

        // Evict oldest until capacity constraints are satisfied for the new entry.
        while self.entries.len() >= RRWB_MAX_ENTRIES
            || self.total_bytes + entry_bytes > RRWB_MAX_TOTAL_BYTES
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(evicted.text.len());
        }

        self.entries.push_back(BufferedWrite {
            path,
            version,
            text,
        });
        self.total_bytes += entry_bytes;
    }

    /// Number of entries currently buffered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the buffer holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Search the buffer for entries within `scope` whose text contains the
    /// query (case-insensitive substring match).
    ///
    /// Returns hits sorted by descending `cosine_score`, where the score is
    /// a crude relevance proxy (match count, bounded to `[0, 1]`). This is
    /// intentionally approximate — the tool layer merges these with
    /// persistent-index hits and re-ranks, and only relevance-equivalent
    /// ordering is needed for Guarantee 2 to hold.
    pub fn search(&self, query: &str, scope: &MemoryPath) -> Vec<SearchHit> {
        let query_lc = query.to_lowercase();
        if query_lc.is_empty() {
            return Vec::new();
        }

        let mut hits: Vec<SearchHit> = Vec::new();
        for entry in &self.entries {
            if !entry.path.starts_with_prefix(scope) {
                continue;
            }
            let text_lc = entry.text.to_lowercase();
            if !text_lc.contains(&query_lc) {
                continue;
            }

            // Crude score: log-scaled match count. One match -> ~0.5, many
            // matches asymptotes toward 1.0.
            let match_count = text_lc.matches(&query_lc).count() as f32;
            let cosine_score = 1.0 - 1.0 / (1.0 + match_count);

            let snippet = build_snippet(&entry.text, &text_lc, &query_lc);
            let byte_end = entry.text.len();

            hits.push(SearchHit {
                path: entry.path.clone(),
                chunk_index: 0,
                version: entry.version,
                locator: Locator::Text {
                    byte_start: 0,
                    byte_end,
                },
                snippet,
                cosine_score,
            });
        }

        hits.sort_by(|a, b| b.cosine_score.total_cmp(&a.cosine_score));
        hits
    }
}

/// Build a snippet centered on the first occurrence of `query_lc` within
/// `text`. `text_lc` is the lowercased form of `text` with the same byte
/// layout (lowercase preserves ASCII byte boundaries — for non-ASCII we
/// fall back to the start of the document).
fn build_snippet(text: &str, text_lc: &str, query_lc: &str) -> String {
    // Try to find the match in the lowercased form and map to the original.
    // `to_lowercase` is not length-preserving in general (e.g. German ß),
    // so verify the byte offset lies on an original char boundary before
    // using it; otherwise, fall back to the document prefix.
    let snippet_source: &str = if text.len() == text_lc.len() {
        if let Some(offset) = text_lc.find(query_lc) {
            let start = offset.saturating_sub(RRWB_SNIPPET_CHARS / 4);
            let mut start = start.min(text.len());
            while start > 0 && !text.is_char_boundary(start) {
                start -= 1;
            }
            &text[start..]
        } else {
            text
        }
    } else {
        text
    };

    snippet_source.chars().take(RRWB_SNIPPET_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use simulacra_types::{MemoryPath, MemoryVersion};

    fn path(s: &str) -> MemoryPath {
        MemoryPath::parse(s).unwrap()
    }

    #[test]
    fn new_buffer_is_empty() {
        let buf = RecentWritesBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn record_then_search_finds_the_entry() {
        let mut buf = RecentWritesBuffer::new();
        let p = path("/var/memory/self/note.md");
        buf.record(p.clone(), MemoryVersion(1), b"hello needle world");
        let scope = path("/var/memory/self");
        let hits = buf.search("needle", &scope);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, p);
    }

    #[test]
    fn oversized_writes_are_not_buffered() {
        let mut buf = RecentWritesBuffer::new();
        let p = path("/var/memory/self/big.md");
        let data = vec![b'x'; RRWB_MAX_BYTES_PER_ENTRY + 1];
        buf.record(p, MemoryVersion(1), &data);
        assert!(buf.is_empty());
    }

    #[test]
    fn scope_filter_excludes_out_of_scope_entries() {
        let mut buf = RecentWritesBuffer::new();
        let inside = path("/var/memory/self/a.md");
        let outside = path("/var/memory/users/b.md");
        buf.record(inside.clone(), MemoryVersion(1), b"needle one");
        buf.record(outside, MemoryVersion(1), b"needle two");
        let scope = path("/var/memory/self");
        let hits = buf.search("needle", &scope);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, inside);
    }

    #[test]
    fn capacity_evicts_oldest_entries() {
        let mut buf = RecentWritesBuffer::new();
        for i in 0..=RRWB_MAX_ENTRIES {
            let p = path(&format!("/var/memory/self/d-{i}.md"));
            buf.record(p, MemoryVersion(1), format!("token-{i}").as_bytes());
        }
        assert_eq!(buf.len(), RRWB_MAX_ENTRIES);
        let scope = path("/var/memory/self");
        // token-0 (the first entry written) should be evicted.
        assert!(buf.search("token-0", &scope).is_empty());
        // token-{MAX} (the last) must still be present.
        assert!(
            !buf.search(&format!("token-{RRWB_MAX_ENTRIES}"), &scope)
                .is_empty()
        );
    }

    #[test]
    fn rewriting_the_same_path_supersedes_previous_content() {
        let mut buf = RecentWritesBuffer::new();
        let p = path("/var/memory/self/rewrite.md");
        buf.record(p.clone(), MemoryVersion(1), b"alpha");
        buf.record(p.clone(), MemoryVersion(2), b"bravo");
        let scope = path("/var/memory/self");
        assert_eq!(buf.len(), 1);
        assert!(buf.search("alpha", &scope).is_empty());
        let hits = buf.search("bravo", &scope);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].version, MemoryVersion(2));
    }

    #[test]
    fn non_utf8_writes_are_silently_dropped() {
        let mut buf = RecentWritesBuffer::new();
        let p = path("/var/memory/self/bin.md");
        buf.record(p, MemoryVersion(1), &[0xff, 0xfe, 0xfd]);
        assert!(buf.is_empty());
    }

    #[test]
    fn total_byte_cap_evicts_to_fit() {
        let mut buf = RecentWritesBuffer::new();
        // Each write is 32 KB; ~33 of these would exceed 1 MB.
        let payload = vec![b'a'; 32 * 1024];
        for i in 0..40 {
            let p = path(&format!("/var/memory/self/b-{i}.md"));
            buf.record(p, MemoryVersion(1), &payload);
        }
        assert!(buf.total_bytes <= RRWB_MAX_TOTAL_BYTES);
        assert!(buf.len() <= RRWB_MAX_ENTRIES);
    }
}
