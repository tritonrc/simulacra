//! Journal replay iterator for the Restate pattern.
//!
//! Walks a recorded journal. Before the frontier, returns stored results.
//! At the frontier, returns `None` so the caller can execute live.

use simulacra_types::{JOURNAL_SCHEMA_VERSION, JournalEntry, JournalEntryKind, JournalError};

/// Iterates over recorded journal entries, yielding stored results
/// until the frontier is reached.
#[derive(Debug)]
pub struct JournalReplayIterator {
    entries: Vec<JournalEntry>,
    cursor: usize,
    schema_mismatch: Option<(u32, u32)>,
}

impl JournalReplayIterator {
    /// Create a new replay iterator from a vec of journal entries.
    pub fn new(entries: Vec<JournalEntry>) -> Self {
        let schema_mismatch = entries
            .iter()
            .find(|entry| entry.schema_version != JOURNAL_SCHEMA_VERSION)
            .map(|entry| (JOURNAL_SCHEMA_VERSION, entry.schema_version));
        Self {
            entries,
            cursor: 0,
            schema_mismatch,
        }
    }

    /// Reject an incompatible typed journal before any entry kind is exposed.
    pub fn validate_schema_version(&self) -> Result<(), JournalError> {
        match self.schema_mismatch {
            Some((expected, got)) => {
                tracing::error!(
                    expected,
                    got,
                    "journal schema version mismatch; start a new session"
                );
                Err(JournalError::SchemaVersionMismatch { expected, got })
            }
            None => Ok(()),
        }
    }

    /// Returns the next recorded entry kind if before the frontier.
    /// Returns `None` if the frontier has been reached (switch to live execution).
    pub fn next_recorded(&mut self) -> Option<&JournalEntryKind> {
        if self.schema_mismatch.is_none() && self.cursor < self.entries.len() {
            let kind = &self.entries[self.cursor].entry;
            self.cursor += 1;
            Some(kind)
        } else {
            None
        }
    }

    /// Peek at the next entry without advancing the cursor.
    pub fn peek(&self) -> Option<&JournalEntryKind> {
        if self.schema_mismatch.is_none() && self.cursor < self.entries.len() {
            Some(&self.entries[self.cursor].entry)
        } else {
            None
        }
    }

    /// Whether the frontier has been reached (no more recorded entries).
    pub fn at_frontier(&self) -> bool {
        self.schema_mismatch.is_some() || self.cursor >= self.entries.len()
    }

    /// How many entries remain before the frontier.
    pub fn remaining(&self) -> usize {
        if self.schema_mismatch.is_some() {
            0
        } else {
            self.entries.len().saturating_sub(self.cursor)
        }
    }

    /// Current cursor position.
    pub fn position(&self) -> usize {
        self.cursor
    }

    /// Access the underlying entries slice for inspection (e.g. checkpoint scanning).
    pub fn entries(&self) -> &[JournalEntry] {
        if self.schema_mismatch.is_some() {
            &[]
        } else {
            &self.entries
        }
    }
}
