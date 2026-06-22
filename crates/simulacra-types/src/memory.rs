//! Core memory types: tenant id, memory paths, capability, locator, hit id.
//!
//! These types live in `simulacra-types` (not `simulacra-memory`) because `MemoryCapability`
//! is a field of `CapabilityToken`. The traits and implementations (MemoryStore,
//! VectorIndex, Embedder, Chunker, SQLite backends) live in `simulacra-memory`.
//!
//! See `specs/S037-memory-and-semantic-retrieval.md` for the full design.

use serde::{Deserialize, Serialize};
use std::fmt;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Maximum size of the snippet returned in a search hit.
pub const MEMORY_SNIPPET_CHARS: usize = 320;

/// Time-to-live for `HitId`s in the per-process cache.
pub const HIT_ID_TTL_SECONDS: u64 = 300;

/// Maximum number of `HitId` cache entries across all tenants in a process.
pub const HIT_ID_CACHE_MAX: usize = 65_536;

/// Maximum entries in the per-run RecentWritesBuffer (RRWB).
pub const RRWB_MAX_ENTRIES: usize = 64;

/// Maximum payload bytes per RRWB entry. Larger writes skip the buffer.
pub const RRWB_MAX_BYTES_PER_ENTRY: usize = 64 * 1024;

/// Maximum total bytes held in the RRWB.
pub const RRWB_MAX_TOTAL_BYTES: usize = 1024 * 1024;

// ─── TenantId ─────────────────────────────────────────────────────────────────

/// Validated tenant identifier. Safe for filesystem path interpolation.
///
/// Pattern: `^[a-z0-9][a-z0-9_-]{0,63}$`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TenantId(String);

#[derive(Debug, thiserror::Error)]
pub enum TenantIdError {
    #[error("tenant id is empty")]
    Empty,
    #[error("tenant id too long: max 64 characters, got {0}")]
    TooLong(usize),
    #[error("tenant id must start with a lowercase letter or digit")]
    InvalidStart,
    #[error("tenant id contains invalid character: '{0}' (allowed: a-z, 0-9, _, -)")]
    InvalidChar(char),
}

impl TenantId {
    /// Parse and validate a tenant id.
    pub fn parse(s: &str) -> Result<Self, TenantIdError> {
        if s.is_empty() {
            return Err(TenantIdError::Empty);
        }
        if s.len() > 64 {
            return Err(TenantIdError::TooLong(s.len()));
        }
        let mut chars = s.chars();
        let first = chars.next().unwrap();
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(TenantIdError::InvalidStart);
        }
        for c in chars {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
                return Err(TenantIdError::InvalidChar(c));
            }
        }
        Ok(TenantId(s.to_string()))
    }

    /// The validated string. Safe to use as a filesystem segment.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Same as `as_str`. Named explicitly for clarity at use sites.
    pub fn as_fs_segment(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for TenantId {
    type Error = TenantIdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        TenantId::parse(&s)
    }
}

impl From<TenantId> for String {
    fn from(t: TenantId) -> String {
        t.0
    }
}

// ─── MemoryPath ───────────────────────────────────────────────────────────────

/// Validated memory path, rooted at `/var/memory/` or `/mnt/`.
///
/// Rejects `..` (NOT collapsed silently like the general VFS path normalizer),
/// null bytes, control characters, oversized segments, and any path that
/// doesn't start with one of the two allowed prefixes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MemoryPath(String);

#[derive(Debug, thiserror::Error)]
pub enum MemoryPathError {
    #[error("memory path is empty")]
    Empty,
    #[error("memory path must start with /var/memory/ or /mnt/")]
    InvalidPrefix,
    #[error("memory path contains '..' segment (parent traversal not allowed)")]
    ParentSegment,
    #[error("memory path contains null byte")]
    NullByte,
    #[error("memory path contains control character: U+{0:04X}")]
    ControlChar(u32),
    #[error("path segment too long: max 255 bytes, got {0}")]
    SegmentTooLong(usize),
    #[error("memory path too long: max 1024 bytes, got {0}")]
    PathTooLong(usize),
    #[error("memory path contains empty segment (consecutive slashes)")]
    EmptySegment,
}

impl MemoryPath {
    /// Parse and validate. Returns the canonical form (no trailing slashes,
    /// no consecutive slashes, no `.` segments).
    pub fn parse(s: &str) -> Result<Self, MemoryPathError> {
        if s.is_empty() {
            return Err(MemoryPathError::Empty);
        }
        if s.len() > 1024 {
            return Err(MemoryPathError::PathTooLong(s.len()));
        }
        if s.contains('\0') {
            return Err(MemoryPathError::NullByte);
        }
        for c in s.chars() {
            if (c as u32) < 0x20 || c == 0x7f as char {
                return Err(MemoryPathError::ControlChar(c as u32));
            }
        }
        if !(s.starts_with("/var/memory/")
            || s == "/var/memory"
            || s.starts_with("/mnt/")
            || s == "/mnt")
        {
            return Err(MemoryPathError::InvalidPrefix);
        }

        // Walk segments. Reject ".." and empty segments. Strip "." segments.
        // Collapse trailing slashes.
        let trimmed = s.trim_end_matches('/');
        let trimmed = if trimmed.is_empty() { s } else { trimmed };
        let mut canonical = String::with_capacity(trimmed.len());
        canonical.push('/');
        let mut first = true;
        for segment in trimmed.split('/').skip(1) {
            if segment.is_empty() {
                return Err(MemoryPathError::EmptySegment);
            }
            if segment == ".." {
                return Err(MemoryPathError::ParentSegment);
            }
            if segment == "." {
                continue;
            }
            if segment.len() > 255 {
                return Err(MemoryPathError::SegmentTooLong(segment.len()));
            }
            if !first {
                canonical.push('/');
            }
            canonical.push_str(segment);
            first = false;
        }

        Ok(MemoryPath(canonical))
    }

    /// The canonical path string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns true if `self` is at or under the given prefix, with
    /// segment-boundary matching. `/var/memory/selfish` is NOT under
    /// `/var/memory/self`.
    pub fn starts_with_prefix(&self, prefix: &MemoryPath) -> bool {
        if self.0 == prefix.0 {
            return true;
        }
        let p = if prefix.0.ends_with('/') {
            prefix.0.clone()
        } else {
            format!("{}/", prefix.0)
        };
        self.0.starts_with(&p)
    }

    /// True if the path is under `/var/memory/dedup/` (which is not indexed).
    pub fn is_dedup(&self) -> bool {
        self.0.starts_with("/var/memory/dedup/") || self.0 == "/var/memory/dedup"
    }

    /// True if the path is under `/mnt/` (admin-ingested).
    pub fn is_mnt(&self) -> bool {
        self.0.starts_with("/mnt/") || self.0 == "/mnt"
    }

    /// True if the raw path string is under `/var/memory/**` or `/mnt/**`.
    ///
    /// This is the **single source of truth** for memory-path classification.
    /// Both the capability layer (`CapabilityToken::check_path_*`) and the VFS
    /// layer (`MemoryStoreFs`) consult this helper. Keeping the definition in
    /// one place prevents the two layers from drifting and opening a gap.
    ///
    /// Matching is segment-aware: `/var/memory.bak/foo` and `/mntfoo/bar` are
    /// NOT memory paths. Case-sensitive: `/Var/Memory/...` is NOT a match.
    pub fn is_memory_path_str(path: &str) -> bool {
        path.starts_with("/var/memory/")
            || path == "/var/memory"
            || path.starts_with("/mnt/")
            || path == "/mnt"
    }
}

impl fmt::Display for MemoryPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for MemoryPath {
    type Error = MemoryPathError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        MemoryPath::parse(&s)
    }
}

impl From<MemoryPath> for String {
    fn from(p: MemoryPath) -> String {
        p.0
    }
}

// ─── MemoryVersion ────────────────────────────────────────────────────────────

/// Monotonic per-path version. Bumped on every put and delete (tombstone).
/// Used by the index upsert path to drop stale embedding work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MemoryVersion(pub u64);

impl MemoryVersion {
    pub const ZERO: MemoryVersion = MemoryVersion(0);
}

impl fmt::Display for MemoryVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── HitId ────────────────────────────────────────────────────────────────────

/// Opaque, unguessable token returned by `semantic_search` and consumed by
/// `memory_read_chunk`. CSPRNG-sourced 24 bytes, base32-encoded (192 bits
/// of entropy). 5-minute TTL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HitId(pub String);

impl fmt::Display for HitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ─── Locator ──────────────────────────────────────────────────────────────────

/// Source-type-aware coordinates for a chunk. Different source types need
/// different addressing — raw byte ranges are wrong for PDF (binary), HTML
/// (stripped before chunking), or JSONL (lines, not bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Locator {
    /// Plain text or Markdown — byte range is valid in the source file.
    Text { byte_start: usize, byte_end: usize },

    /// PDF — page number (1-indexed) and paragraph ordinal within the page.
    PdfPage { page: u32, paragraph: u32 },

    /// HTML — DOM path (CSS selector form) and byte range within extracted text.
    HtmlSelector {
        selector: String,
        text_start: usize,
        text_end: usize,
    },

    /// JSONL or NDJSON — 0-indexed line number.
    JsonlLine { line: u64 },

    /// Opaque — locator the source format understands, carried through but
    /// not interpreted by the index. The `format` discriminator names the
    /// upstream format (e.g. "epub", "docx").
    Opaque { format: String, payload: String },
}

// ─── MemoryCapability ─────────────────────────────────────────────────────────

/// Memory capability section on `CapabilityToken`. Default: disabled. Memory
/// access is opt-in per agent type. The permissive engine fallback for
/// untyped agents MUST keep this disabled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCapability {
    /// If false, semantic_search and memory_read_chunk are not registered,
    /// and MemoryStoreFs is not installed in the VFS stack.
    pub enabled: bool,
    /// Prefixes the agent can search. Each must be a valid MemoryPath.
    pub search_scopes: Vec<MemoryPath>,
    /// Prefixes the agent can write to. Each must be a valid MemoryPath.
    pub write_scopes: Vec<MemoryPath>,
}

impl MemoryCapability {
    /// True if the given path is inside any `search_scopes` entry.
    pub fn can_read(&self, path: &MemoryPath) -> bool {
        self.enabled
            && self
                .search_scopes
                .iter()
                .any(|scope| path.starts_with_prefix(scope))
    }

    /// True if the given path is inside any `write_scopes` entry.
    pub fn can_write(&self, path: &MemoryPath) -> bool {
        self.enabled
            && self
                .write_scopes
                .iter()
                .any(|scope| path.starts_with_prefix(scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TenantId ──

    #[test]
    fn tenant_id_accepts_valid_format() {
        assert!(TenantId::parse("acme").is_ok());
        assert!(TenantId::parse("acme-corp").is_ok());
        assert!(TenantId::parse("acme_corp").is_ok());
        assert!(TenantId::parse("a1b2c3").is_ok());
        assert!(TenantId::parse("0").is_ok());
        assert!(TenantId::parse(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn tenant_id_rejects_empty() {
        assert!(matches!(TenantId::parse(""), Err(TenantIdError::Empty)));
    }

    #[test]
    fn tenant_id_rejects_too_long() {
        assert!(matches!(
            TenantId::parse(&"a".repeat(65)),
            Err(TenantIdError::TooLong(65))
        ));
    }

    #[test]
    fn tenant_id_rejects_uppercase() {
        assert!(matches!(
            TenantId::parse("Acme"),
            Err(TenantIdError::InvalidStart)
        ));
        assert!(matches!(
            TenantId::parse("acmE"),
            Err(TenantIdError::InvalidChar('E'))
        ));
    }

    #[test]
    fn tenant_id_rejects_path_separators() {
        assert!(matches!(
            TenantId::parse("acme/corp"),
            Err(TenantIdError::InvalidChar('/'))
        ));
        assert!(matches!(
            TenantId::parse("../etc"),
            Err(TenantIdError::InvalidStart)
        ));
        assert!(matches!(
            TenantId::parse("acme.corp"),
            Err(TenantIdError::InvalidChar('.'))
        ));
    }

    #[test]
    fn tenant_id_rejects_starting_dash_or_underscore() {
        assert!(matches!(
            TenantId::parse("-acme"),
            Err(TenantIdError::InvalidStart)
        ));
        assert!(matches!(
            TenantId::parse("_acme"),
            Err(TenantIdError::InvalidStart)
        ));
    }

    // ── MemoryPath ──

    #[test]
    fn memory_path_accepts_valid() {
        assert!(MemoryPath::parse("/var/memory/self/note.md").is_ok());
        assert!(MemoryPath::parse("/var/memory/users/brian.md").is_ok());
        assert!(MemoryPath::parse("/mnt/policies/hr.pdf").is_ok());
        assert!(MemoryPath::parse("/var/memory/entities/customers/X.md").is_ok());
    }

    #[test]
    fn memory_path_rejects_parent_traversal() {
        assert!(matches!(
            MemoryPath::parse("/var/memory/self/../users/x.md"),
            Err(MemoryPathError::ParentSegment)
        ));
        assert!(matches!(
            MemoryPath::parse("/var/memory/.."),
            Err(MemoryPathError::ParentSegment)
        ));
    }

    #[test]
    fn memory_path_rejects_invalid_prefix() {
        assert!(matches!(
            MemoryPath::parse("/etc/passwd"),
            Err(MemoryPathError::InvalidPrefix)
        ));
        assert!(matches!(
            MemoryPath::parse("/workspace/note.md"),
            Err(MemoryPathError::InvalidPrefix)
        ));
        assert!(matches!(
            MemoryPath::parse("var/memory/x.md"),
            Err(MemoryPathError::InvalidPrefix)
        ));
    }

    #[test]
    fn memory_path_rejects_null_and_control_chars() {
        assert!(matches!(
            MemoryPath::parse("/var/memory/x\0y.md"),
            Err(MemoryPathError::NullByte)
        ));
        assert!(matches!(
            MemoryPath::parse("/var/memory/x\ny.md"),
            Err(MemoryPathError::ControlChar(_))
        ));
    }

    #[test]
    fn memory_path_collapses_trailing_slash() {
        let p = MemoryPath::parse("/var/memory/self/").unwrap();
        assert_eq!(p.as_str(), "/var/memory/self");
    }

    #[test]
    fn memory_path_segment_boundary_prefix_match() {
        let prefix = MemoryPath::parse("/var/memory/self").unwrap();
        let inside = MemoryPath::parse("/var/memory/self/note.md").unwrap();
        let lookalike = MemoryPath::parse("/var/memory/selfish/note.md").unwrap();
        assert!(inside.starts_with_prefix(&prefix));
        assert!(!lookalike.starts_with_prefix(&prefix));
    }

    #[test]
    fn memory_path_exact_match_is_prefix() {
        let p = MemoryPath::parse("/var/memory/self").unwrap();
        assert!(p.starts_with_prefix(&p));
    }

    #[test]
    fn memory_path_dedup_detection() {
        let dedup = MemoryPath::parse("/var/memory/dedup/foo").unwrap();
        let other = MemoryPath::parse("/var/memory/self/foo.md").unwrap();
        assert!(dedup.is_dedup());
        assert!(!other.is_dedup());
    }

    // ── MemoryCapability ──

    #[test]
    fn memory_capability_default_disabled() {
        let cap = MemoryCapability::default();
        assert!(!cap.enabled);
        assert!(cap.search_scopes.is_empty());
        assert!(cap.write_scopes.is_empty());
    }

    #[test]
    fn memory_capability_disabled_blocks_everything() {
        let cap = MemoryCapability::default();
        let path = MemoryPath::parse("/var/memory/self/x.md").unwrap();
        assert!(!cap.can_read(&path));
        assert!(!cap.can_write(&path));
    }

    #[test]
    fn memory_capability_enforces_scopes() {
        let cap = MemoryCapability {
            enabled: true,
            search_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
            write_scopes: vec![MemoryPath::parse("/var/memory/self").unwrap()],
        };
        let inside = MemoryPath::parse("/var/memory/self/x.md").unwrap();
        let outside = MemoryPath::parse("/var/memory/users/x.md").unwrap();
        assert!(cap.can_read(&inside));
        assert!(cap.can_write(&inside));
        assert!(!cap.can_read(&outside));
        assert!(!cap.can_write(&outside));
    }
}
