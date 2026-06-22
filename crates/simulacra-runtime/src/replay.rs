//! Journal replay iterator for the Restate pattern.
//!
//! Walks a recorded journal. Before the frontier, returns stored results.
//! At the frontier, returns `None` so the caller can execute live.

use simulacra_types::{JournalEntry, JournalEntryKind};

/// Iterates over recorded journal entries, yielding stored results
/// until the frontier is reached.
#[derive(Debug)]
pub struct JournalReplayIterator {
    entries: Vec<JournalEntry>,
    cursor: usize,
}

impl JournalReplayIterator {
    /// Create a new replay iterator from a vec of journal entries.
    pub fn new(entries: Vec<JournalEntry>) -> Self {
        Self { entries, cursor: 0 }
    }

    /// Returns the next recorded entry kind if before the frontier.
    /// Returns `None` if the frontier has been reached (switch to live execution).
    pub fn next_recorded(&mut self) -> Option<&JournalEntryKind> {
        if self.cursor < self.entries.len() {
            let kind = &self.entries[self.cursor].entry;
            self.cursor += 1;
            Some(kind)
        } else {
            None
        }
    }

    /// Peek at the next entry without advancing the cursor.
    pub fn peek(&self) -> Option<&JournalEntryKind> {
        if self.cursor < self.entries.len() {
            Some(&self.entries[self.cursor].entry)
        } else {
            None
        }
    }

    /// Whether the frontier has been reached (no more recorded entries).
    pub fn at_frontier(&self) -> bool {
        self.cursor >= self.entries.len()
    }

    /// How many entries remain before the frontier.
    pub fn remaining(&self) -> usize {
        self.entries.len().saturating_sub(self.cursor)
    }

    /// Current cursor position.
    pub fn position(&self) -> usize {
        self.cursor
    }

    /// Access the underlying entries slice for inspection (e.g. checkpoint scanning).
    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }
}
